//! EROFS-shaped LZ4HC benchmark. Run with:
//! `cargo bench --bench erofs_lz4hc`
//!
//! The hot path mirrors fixed-output EROFS compression: offer a source
//! window to an output cluster, advance by the returned source length only
//! when compression saves a block, and otherwise store one plain block and
//! retry from there. This makes incompressible-input retries visible, unlike
//! benchmarks that compress one whole buffer to a Vec.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const BLOCK_SIZE: usize = 4 * 1024;
// Match mkfs::COMPRESS_WINDOW, which offers up to one MiB to each pcluster.
const SOURCE_WINDOW: usize = 1 << 20;
const OUTPUT_CLUSTERS: [usize; 4] = [4 * 1024, 8 * 1024, 16 * 1024, 32 * 1024];
const DEFAULT_REPEATS: usize = 3;
// Keep each sample long enough to drown out timer granularity and short-lived
// runner noise. External corpora still run at least once, then repeat whole
// passes until they reach this minimum duration.
const MIN_SAMPLE_TIME: Duration = Duration::from_millis(250);

struct ResultStats {
    elapsed: Duration,
    input_bytes: u64,
    output_bytes: u64,
    offered_bytes: u64,
    attempted_bytes: u64,
    calls: u64,
    compressed_clusters: u64,
    plain_fallbacks: u64,
    repetitions: u64,
}

fn main() {
    // `cargo bench` passes `--bench` even for harness-free targets.
    let args: Vec<_> = env::args_os()
        .skip(1)
        .filter(|arg| arg != "--bench")
        .collect();
    let input_dir = match args.as_slice() {
        [] => None,
        [flag, path] if flag == "--input-dir" => Some(PathBuf::from(path)),
        _ => fail("usage: cargo bench --bench erofs_lz4hc [-- --input-dir DIR]"),
    };
    let level = env::var("EROFS_LZ4HC_LEVEL")
        .map(|value| {
            value
                .parse::<u8>()
                .unwrap_or_else(|_| fail("EROFS_LZ4HC_LEVEL must be an integer from 1 to 12"))
        })
        .unwrap_or(12);
    if !(1..=12).contains(&level) {
        fail("EROFS_LZ4HC_LEVEL must be an integer from 1 to 12");
    }

    println!(
        "environment,os={},arch={},rustc={},cpu={},erofs_rev={},lz4_flex_rev={},lz4_hc_level={level}",
        env::consts::OS,
        env::consts::ARCH,
        rustc_version(),
        cpu_model(),
        git_revision("."),
        env::var("LZ4_FLEX_REV").unwrap_or_else(|_| "Cargo.lock-pinned".to_string()),
    );
    let external_corpus = input_dir.is_some();
    let corpora = match input_dir {
        Some(path) => load_corpus(&path),
        None => synthetic_corpus(),
    };
    let repeats = if external_corpus { 1 } else { DEFAULT_REPEATS };
    #[cfg(feature = "bench-reuse")]
    let mut rust_table = lz4_flex_hc::block::CompressHcDestSizeTable::new();

    for (name, data) in corpora {
        for output_capacity in OUTPUT_CLUSTERS {
            #[cfg(feature = "bench-reuse")]
            let per_call_workspace_bytes = {
                let mut workspace_probe = lz4_flex_hc::block::CompressHcDestSizeTable::new();
                let mut probe_output = vec![0u8; output_capacity];
                let probe_input = &data[..data.len().min(SOURCE_WINDOW)];
                lz4_flex_hc::block::compress_hc_dest_size_with_table(
                    probe_input,
                    &mut probe_output,
                    level,
                    &mut workspace_probe,
                );
                workspace_probe.memory_usage_bytes()
            };

            // Keep a correctness alarm beside the timing: valid-looking byte
            // counts are not enough if either encoder emits an invalid block.
            verify_round_trip(&name, "lz4_flex", &data, output_capacity, |src, dst| {
                lz4_flex_hc::block::compress_hc_dest_size(src, dst, level)
            });
            #[cfg(feature = "bench-reuse")]
            verify_round_trip(
                &name,
                "lz4_flex_reused",
                &data,
                output_capacity,
                |src, dst| {
                    lz4_flex_hc::block::compress_hc_dest_size_with_table(
                        src,
                        dst,
                        level,
                        &mut rust_table,
                    )
                },
            );
            verify_round_trip(&name, "liblz4", &data, output_capacity, |src, dst| {
                lzzzz::lz4_hc::compress_fill(src, dst, i32::from(level))
                    .expect("liblz4 fixed-output compression")
            });
            println!(
                "verified,corpus={name},corpus_bytes={},level={level},output_capacity={output_capacity}",
                data.len()
            );

            let stateless = measure(repeats, &data, output_capacity, |src, dst| {
                lz4_flex_hc::block::compress_hc_dest_size(src, dst, level)
            });

            #[cfg(feature = "bench-reuse")]
            let reused = measure(repeats, &data, output_capacity, |src, dst| {
                lz4_flex_hc::block::compress_hc_dest_size_with_table(
                    src,
                    dst,
                    level,
                    &mut rust_table,
                )
            });

            let c = measure(repeats, &data, output_capacity, |src, dst| {
                lzzzz::lz4_hc::compress_fill(src, dst, i32::from(level))
                    .expect("liblz4 fixed-output compression")
            });

            let stateless_ratio_delta = compression_ratio(&stateless) - compression_ratio(&c);
            let ratio_budget = 0.004 + BLOCK_SIZE as f64 / data.len() as f64;
            assert!(
                stateless_ratio_delta <= ratio_budget,
                "lz4_flex_stateless/{name} stores {:.3}% more than liblz4; budget is {:.3}%",
                stateless_ratio_delta * 100.0,
                ratio_budget * 100.0,
            );
            #[cfg(feature = "bench-reuse")]
            let reused_ratio_delta = compression_ratio(&reused) - compression_ratio(&c);
            #[cfg(feature = "bench-reuse")]
            assert!(
                reused_ratio_delta <= ratio_budget,
                "lz4_flex_reused/{name} stores {:.3}% more than liblz4; budget is {:.3}%",
                reused_ratio_delta * 100.0,
                ratio_budget * 100.0,
            );

            if !external_corpus && name == "compressed_asset_proxy" && output_capacity > BLOCK_SIZE
            {
                assert!(
                    stateless.plain_fallbacks > 0 && c.plain_fallbacks > 0,
                    "the incompressible asset proxy must exercise EROFS plain-block retries"
                );
                #[cfg(feature = "bench-reuse")]
                assert!(reused.plain_fallbacks > 0);
            }

            print_result(
                &name,
                "lz4_flex_stateless",
                level,
                output_capacity,
                data.len(),
                &stateless,
            );
            #[cfg(feature = "bench-reuse")]
            print_result(
                &name,
                "lz4_flex_reused",
                level,
                output_capacity,
                data.len(),
                &reused,
            );
            #[cfg(feature = "bench-reuse")]
            println!(
                "memory,corpus={name},codec=lz4_flex,transient_workspace_bytes={per_call_workspace_bytes},reused_retained_workspace_bytes={}",
                rust_table.memory_usage_bytes(),
            );
            print_result(&name, "liblz4", level, output_capacity, data.len(), &c);
            println!(
                "comparison,corpus={name},corpus_bytes={},level={level},output_capacity={output_capacity},stateless_speedup_vs_liblz4={:.3},stateless_ratio_delta={:.5}",
                data.len(),
                throughput_mib_per_sec(&stateless) / throughput_mib_per_sec(&c),
                stateless_ratio_delta
            );
            #[cfg(feature = "bench-reuse")]
            println!(
                "reused_comparison,corpus={name},corpus_bytes={},level={level},output_capacity={output_capacity},reused_speedup_vs_liblz4={:.3},reused_speedup_vs_stateless={:.3},reused_ratio_delta={:.5}",
                data.len(),
                throughput_mib_per_sec(&reused) / throughput_mib_per_sec(&c),
                throughput_mib_per_sec(&reused) / throughput_mib_per_sec(&stateless),
                reused_ratio_delta
            );
        }
    }
}

fn measure<F>(repeats: usize, data: &[u8], capacity: usize, mut compress: F) -> ResultStats
where
    F: FnMut(&[u8], &mut [u8]) -> (usize, usize),
{
    let mut output = vec![0u8; capacity];
    let start = Instant::now();
    let mut input_bytes = 0u64;
    let mut output_bytes = 0u64;
    let mut offered_bytes = 0u64;
    let mut attempted_bytes = 0u64;
    let mut calls = 0u64;
    let mut compressed_clusters = 0u64;
    let mut plain_fallbacks = 0u64;
    let mut repetitions = 0u64;

    while repetitions < repeats as u64 || start.elapsed() < MIN_SAMPLE_TIME {
        let mut offset = 0usize;
        while offset < data.len() {
            let remaining = &data[offset..];
            let window = &remaining[..remaining.len().min(SOURCE_WINDOW)];
            let (consumed, written) = compress(window, &mut output);
            assert!(
                consumed > 0 && consumed <= window.len(),
                "invalid consumed length {consumed}"
            );
            assert!(
                written > 0 && written <= output.len(),
                "invalid output length {written}"
            );
            let (advance, compressed) = erofs_piece_len(offset, remaining.len(), consumed, written);
            input_bytes += advance as u64;
            offered_bytes += window.len() as u64;
            attempted_bytes += consumed as u64;
            if compressed {
                output_bytes += stored_size(written) as u64;
                compressed_clusters += 1;
            } else {
                output_bytes += stored_size(advance) as u64;
                plain_fallbacks += 1;
            }
            offset += advance;
            calls += 1;
        }
        repetitions += 1;
    }

    assert_eq!(
        input_bytes,
        (data.len() as u64) * repetitions,
        "benchmark did not consume the entire corpus"
    );
    assert!(
        output_bytes > 0 && calls > 0,
        "benchmark produced no blocks"
    );

    ResultStats {
        elapsed: start.elapsed(),
        input_bytes,
        output_bytes,
        offered_bytes,
        attempted_bytes,
        calls,
        compressed_clusters,
        plain_fallbacks,
        repetitions,
    }
}

fn verify_round_trip<F>(corpus: &str, codec: &str, data: &[u8], capacity: usize, mut compress: F)
where
    F: FnMut(&[u8], &mut [u8]) -> (usize, usize),
{
    let mut output = vec![0u8; capacity];
    let mut offset = 0usize;
    while offset < data.len() {
        let remaining = &data[offset..];
        let window = &remaining[..remaining.len().min(SOURCE_WINDOW)];
        let (consumed, written) = compress(window, &mut output);
        assert!(
            consumed > 0 && consumed <= window.len(),
            "{codec}/{corpus}: invalid consumed length {consumed}"
        );
        assert!(
            written > 0 && written <= output.len(),
            "{codec}/{corpus}: invalid output length {written}"
        );

        let expected = &data[offset..offset + consumed];

        // Treat liblz4's safe block decoder as an independent format oracle
        // for every encoder, including lz4_flex. The reverse check below
        // ensures liblz4-produced blocks remain readable by our Rust decoder.
        let mut c_decoded = vec![0u8; consumed];
        let c_decoded_len = lzzzz::lz4::decompress(&output[..written], &mut c_decoded)
            .unwrap_or_else(|error| panic!("{codec}/{corpus}: liblz4 rejected LZ4 block: {error}"));
        assert_eq!(
            c_decoded_len, consumed,
            "{codec}/{corpus}: liblz4 decoded a different output length"
        );
        assert_eq!(
            c_decoded, expected,
            "{codec}/{corpus}: liblz4 decoded different bytes"
        );

        let mut rust_decoded = vec![0u8; consumed];
        let rust_decoded_len =
            lz4_flex::block::decompress_into(&output[..written], &mut rust_decoded).unwrap_or_else(
                |error| panic!("{codec}/{corpus}: lz4_flex rejected LZ4 block: {error}"),
            );
        assert_eq!(
            rust_decoded_len, consumed,
            "{codec}/{corpus}: lz4_flex decoded a different output length"
        );
        assert_eq!(
            rust_decoded, expected,
            "{codec}/{corpus}: lz4_flex decoded different bytes"
        );
        let (advance, _) = erofs_piece_len(offset, remaining.len(), consumed, written);
        offset += advance;
    }
}

fn erofs_piece_len(pos: usize, remaining: usize, taken: usize, written: usize) -> (usize, bool) {
    let compressed_blocks = written.div_ceil(BLOCK_SIZE);
    let input_blocks = taken.div_ceil(BLOCK_SIZE);
    if taken > 0 && compressed_blocks < input_blocks {
        return (taken, true);
    }

    let into = pos % BLOCK_SIZE;
    let plain_len = if remaining > BLOCK_SIZE || into == 0 {
        BLOCK_SIZE.min(remaining)
    } else {
        remaining.min(BLOCK_SIZE - into)
    };
    (plain_len, false)
}

fn stored_size(bytes: usize) -> usize {
    bytes.div_ceil(BLOCK_SIZE) * BLOCK_SIZE
}

fn print_result(
    corpus: &str,
    codec: &str,
    level: u8,
    capacity: usize,
    corpus_bytes: usize,
    stats: &ResultStats,
) {
    let mib_per_sec = throughput_mib_per_sec(stats);
    println!(
        "result,corpus={corpus},codec={codec},level={level},corpus_bytes={corpus_bytes},source_window={SOURCE_WINDOW},output_capacity={capacity},input_bytes={},stored_bytes={},ratio={:.5},offered_bytes={},attempted_bytes={},calls={},compressed_clusters={},plain_fallbacks={},repetitions={},seconds={:.6},mib_per_sec={mib_per_sec:.3}",
        stats.input_bytes,
        stats.output_bytes,
        stats.output_bytes as f64 / stats.input_bytes as f64,
        stats.offered_bytes,
        stats.attempted_bytes,
        stats.calls,
        stats.compressed_clusters,
        stats.plain_fallbacks,
        stats.repetitions,
        stats.elapsed.as_secs_f64(),
    );
}

fn throughput_mib_per_sec(stats: &ResultStats) -> f64 {
    stats.input_bytes as f64 / (1024.0 * 1024.0) / stats.elapsed.as_secs_f64()
}

fn compression_ratio(stats: &ResultStats) -> f64 {
    stats.output_bytes as f64 / stats.input_bytes as f64
}

fn synthetic_corpus() -> Vec<(String, Vec<u8>)> {
    let mut rng = XorShift64(0x8f3d_91a7_c5e2_4b61);
    let mut incompressible = vec![0; 256 * 1024];
    rng.fill(&mut incompressible);

    let mut native_like = Vec::with_capacity(1 << 20);
    for i in 0..(1 << 20) {
        // Repeated instruction-like bytes with small, high-entropy patches.
        let byte = match i % 97 {
            0..=31 => rng.next() as u8,
            32..=95 => [0x55, 0x48, 0x89, 0xe5, 0x48, 0x83, 0xec, 0x20][i % 8],
            _ => (i / 97) as u8,
        };
        native_like.push(byte);
    }

    let source_line = b"pub fn decode_cluster(input: &[u8], block_size: usize) -> Result<Vec<u8>> {\n    let mut output = vec![0; block_size];\n    decode_into(input, &mut output)?;\n    Ok(output)\n}\n";
    let mut source_like = Vec::with_capacity(1 << 20);
    while source_like.len() < 1 << 20 {
        source_like.extend_from_slice(source_line);
    }
    source_like.truncate(1 << 20);

    let mut mixed = source_like.clone();
    for (index, byte) in mixed.iter_mut().enumerate() {
        if index % 4 == 0 {
            *byte = rng.next() as u8;
        }
    }

    // This is the regression input from lz4_flex PR #3: high entropy, then a
    // long zero run, then short repeated ELF directory entries. It used to
    // leave HC search positions linked to a match beyond the 64 KiB window.
    let mut corpora = vec![
        ("compressed_asset_proxy".into(), incompressible),
        ("source_text".into(), source_like[..256 * 1024].to_vec()),
    ];
    add_size_variants(&mut corpora, "native_code_proxy", &native_like);
    add_size_variants(&mut corpora, "mixed", &mixed);
    for (label, size) in [("64k", 64 * 1024), ("256k", 256 * 1024), ("1m", 1 << 20)] {
        corpora.push((format!("hc_long_run_{label}"), hc_long_run_input(size)));
    }
    // Preserve the exact regression input from lz4_flex PR #3 as its own case.
    corpora.push((
        "hc_long_run_regression".into(),
        hc_long_run_input(4096 + 100_000 + 192),
    ));
    corpora
}

fn add_size_variants(corpora: &mut Vec<(String, Vec<u8>)>, name: &str, data: &[u8]) {
    for (label, size) in [("64k", 64 * 1024), ("256k", 256 * 1024), ("1m", 1 << 20)] {
        corpora.push((format!("{name}_{label}"), data[..size].to_vec()));
    }
}

fn hc_long_run_input(total_size: usize) -> Vec<u8> {
    const PREFIX_SIZE: usize = 4096;
    const ENTRY: [u8; 24] = [
        0x5e, 0x3c, 0x03, 0, 0, 0, 0, 0, 0x5c, 0, 0, 0, 0, 0, 0, 0, 0x12, 0x03, 0, 0, 0x20, 0, 0, 0,
    ];
    const ENTRY_RUN: usize = ENTRY.len() * 8;
    assert!(total_size >= PREFIX_SIZE + ENTRY_RUN);

    let mut data = vec![0; PREFIX_SIZE];
    XorShift64(1).fill(&mut data);
    data.resize(total_size - ENTRY_RUN, 0);
    for _ in 0..8 {
        data.extend_from_slice(&ENTRY);
    }
    data
}

fn load_corpus(root: &Path) -> Vec<(String, Vec<u8>)> {
    let mut paths = Vec::new();
    collect_files(root, &mut paths);
    paths.sort();
    assert!(
        !paths.is_empty(),
        "input corpus is empty: {}",
        root.display()
    );
    paths
        .into_iter()
        .map(|path| {
            let data =
                fs::read(&path).unwrap_or_else(|e| fail(&format!("{}: {e}", path.display())));
            assert!(!data.is_empty(), "empty corpus file: {}", path.display());
            (path.strip_prefix(root).unwrap().display().to_string(), data)
        })
        .collect()
}

fn collect_files(path: &Path, out: &mut Vec<PathBuf>) {
    let entries = fs::read_dir(path).unwrap_or_else(|e| fail(&format!("{}: {e}", path.display())));
    for entry in entries {
        let entry = entry.unwrap_or_else(|e| fail(&e.to_string()));
        let file_type = entry.file_type().unwrap_or_else(|e| fail(&e.to_string()));
        if file_type.is_dir() {
            collect_files(&entry.path(), out);
        } else if file_type.is_file() {
            out.push(entry.path());
        }
    }
}

struct XorShift64(u64);

impl XorShift64 {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    fn fill(&mut self, output: &mut [u8]) {
        for byte in output {
            *byte = self.next() as u8;
        }
    }
}

fn rustc_version() -> String {
    std::process::Command::new("rustc")
        .arg("--version")
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string())
        .unwrap_or_else(|| "unknown".to_string())
}

fn cpu_model() -> String {
    let info = fs::read_to_string("/proc/cpuinfo").unwrap_or_default();
    info.lines()
        .find_map(|line| {
            let (key, value) = line.split_once(':')?;
            matches!(key.trim(), "model name" | "Hardware" | "Processor")
                .then(|| value.trim().replace(',', ";"))
        })
        .unwrap_or_else(|| "unknown".to_string())
}

fn git_revision(path: &str) -> String {
    std::process::Command::new("git")
        .args(["-C", path, "rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string())
        .unwrap_or_else(|| "unknown".to_string())
}

fn fail(message: &str) -> ! {
    eprintln!("erofs_lz4hc: {message}");
    std::process::exit(2)
}
