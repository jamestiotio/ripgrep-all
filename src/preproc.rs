use crate::adapters::*;
use crate::config::RgaConfig;
use crate::recurse::concat_read_streams;
use crate::{matching::*, recurse::RecursingConcattyReader};
use crate::{
    preproc_cache::{LmdbCache, PreprocCache},
    print_bytes, print_dur, CachingReader,
};
use anyhow::*;
use log::*;
use path_clean::PathClean;
// use postproc::PostprocPrefix;
use std::convert::TryInto;
use std::path::Path;
use tokio::io::AsyncBufReadExt;
use tokio::io::BufReader;
use tokio::io::{AsyncBufRead, AsyncRead};

use std::{rc::Rc, time::Instant};

type ActiveAdapters = Vec<(Rc<dyn FileAdapter>)>;

async fn choose_adapter(
    config: &RgaConfig,
    filepath_hint: &Path,
    archive_recursion_depth: i32,
    mut inp: &mut (impl AsyncBufRead + Unpin),
) -> Result<Option<(Rc<dyn FileAdapter>, FileMatcher, ActiveAdapters)>> {
    let active_adapters = get_adapters_filtered(config.custom_adapters.clone(), &config.adapters)?;
    let adapters = adapter_matcher(&active_adapters, config.accurate)?;
    let filename = filepath_hint
        .file_name()
        .ok_or_else(|| format_err!("Empty filename"))?;
    debug!("Archive recursion depth: {}", archive_recursion_depth);

    let mimetype = if config.accurate {
        let buf = inp.fill_buf().await?; // fill but do not consume!
        let mimetype = tree_magic::from_u8(buf);
        debug!("mimetype: {:?}", mimetype);
        Some(mimetype)
    } else {
        None
    };
    let adapter = adapters(FileMeta {
        mimetype,
        lossy_filename: filename.to_string_lossy().to_string(),
    });
    Ok(adapter.map(|e| (e.0, e.1, active_adapters)))
}
/**
 * preprocess a file as defined in `ai`.
 *
 * If a cache is passed, read/write to it.
 *
 */
pub async fn rga_preproc(ai: AdaptInfo<'_>) -> Result<ReadBox<'_>> {
    debug!("path (hint) to preprocess: {:?}", ai.filepath_hint);
    /*todo: move if archive_recursion_depth >= config.max_archive_recursion.0 {
        let s = format!("{}[rga: max archive recursion reached]", line_prefix).into_bytes();
        return Ok(Box::new(std::io::Cursor::new(s)));
    }*/

    // todo: figure out when using a bufreader is a good idea and when it is not
    // seems to be good for File::open() reads, but not sure about within archives (tar, zip)
    let mut inp = BufReader::with_capacity(1 << 16, ai.inp);
    let adapter = choose_adapter(
        &ai.config,
        &ai.filepath_hint,
        ai.archive_recursion_depth,
        &mut inp,
    )
    .await?;
    let (adapter, detection_reason, active_adapters) = match adapter {
        Some((a, d, e)) => (a, d, e),
        None => {
            // allow passthrough if the file is in an archive or accurate matching is enabled
            // otherwise it should have been filtered out by rg pre-glob since rg can handle those better than us
            let allow_cat = !ai.is_real_file || ai.config.accurate;
            if allow_cat {
                if ai.postprocess {
                    panic!("not implemented");
                    /*  (
                        Rc::new(PostprocPrefix {}) as Rc<dyn FileAdapter>,
                        FileMatcher::Fast(FastFileMatcher::FileExtension("default".to_string())), // todo: separate enum value for this
                    )*/
                } else {
                    return Ok(Box::pin(inp));
                }
            } else {
                return Err(format_err!(
                    "No adapter found for file {:?}, passthrough disabled.",
                    ai.filepath_hint
                        .file_name()
                        .ok_or_else(|| format_err!("Empty filename"))?
                ));
            }
        }
    };
    let path_hint_copy = ai.filepath_hint.clone();
    run_adapter_recursively(
        AdaptInfo {
            inp: Box::pin(inp),
            ..ai
        },
        adapter,
        detection_reason,
        active_adapters,
    )
    .await
    .with_context(|| format!("run_adapter({})", &path_hint_copy.to_string_lossy()))
}

fn compute_cache_key(
    filepath_hint: &Path,
    adapter: &dyn FileAdapter,
    active_adapters: ActiveAdapters,
) -> Result<Vec<u8>> {
    let clean_path = filepath_hint.to_owned().clean();
    let meta = std::fs::metadata(&filepath_hint)
        .with_context(|| format!("reading metadata for {}", filepath_hint.to_string_lossy()))?;
    let modified = meta.modified().expect("weird OS that can't into mtime");

    if adapter.metadata().recurses {
        let active_adapters_cache_key = active_adapters
            .iter()
            .map(|a| (a.metadata().name.clone(), a.metadata().version))
            .collect::<Vec<_>>();
        let key = (active_adapters_cache_key, clean_path, modified);
        debug!("Cache key (with recursion): {:?}", key);
        bincode::serialize(&key).context("could not serialize path")
    } else {
        let key = (
            adapter.metadata().name.clone(),
            adapter.metadata().version,
            clean_path,
            modified,
        );
        debug!("Cache key (no recursion): {:?}", key);
        bincode::serialize(&key).context("could not serialize path")
    }
}
async fn run_adapter_recursively<'a>(
    ai: AdaptInfo<'a>,
    adapter: Rc<dyn FileAdapter>,
    detection_reason: FileMatcher,
    active_adapters: ActiveAdapters,
) -> Result<ReadBox<'a>> {
    let AdaptInfo {
        filepath_hint,
        is_real_file,
        inp,
        line_prefix,
        config,
        archive_recursion_depth,
        postprocess,
    } = ai;
    let meta = adapter.metadata();
    debug!(
        "Chose adapter '{}' because of matcher {:?}",
        &meta.name, &detection_reason
    );
    eprintln!(
        "{} adapter: {}",
        filepath_hint.to_string_lossy(),
        &meta.name
    );
    let db_name = format!("{}.v{}", meta.name, meta.version);
    let cache_compression_level = config.cache.compression_level;
    let cache_max_blob_len = config.cache.max_blob_len;

    let cache = if is_real_file {
        LmdbCache::open(&config.cache)?
    } else {
        None
    };

    let mut cache = cache.context("No cache?")?;
    let cache_key: Vec<u8> = compute_cache_key(&filepath_hint, adapter.as_ref(), active_adapters)?;
    // let dbg_ctx = format!("adapter {}", &adapter.metadata().name);
    let cached = cache.get(&db_name, &cache_key)?;
    match cached {
        Some(cached) => Ok(Box::pin(
            async_compression::tokio::bufread::ZstdDecoder::new(std::io::Cursor::new(cached)),
        )),
        None => {
            debug!("cache MISS, running adapter");
            debug!("adapting with caching...");
            let inp = adapter
                .adapt(
                    AdaptInfo {
                        line_prefix,
                        filepath_hint: filepath_hint.clone(),
                        is_real_file,
                        inp,
                        archive_recursion_depth,
                        config,
                        postprocess,
                    },
                    &detection_reason,
                )
                .with_context(|| {
                    format!(
                        "adapting {} via {} failed",
                        filepath_hint.to_string_lossy(),
                        meta.name
                    )
                })?;
            let inp = concat_read_streams(inp);
            let inp = CachingReader::new(
                inp,
                cache_max_blob_len.0.try_into().unwrap(),
                cache_compression_level.0.try_into().unwrap(),
                Box::new(move |(uncompressed_size, compressed)| {
                    debug!(
                        "uncompressed output: {}",
                        print_bytes(uncompressed_size as f64)
                    );
                    if let Some(cached) = compressed {
                        debug!("compressed output: {}", print_bytes(cached.len() as f64));
                        cache.set(&db_name, &cache_key, &cached)?
                    }
                    Ok(())
                }),
            )?;

            Ok(Box::pin(inp))
        }
    }
}
