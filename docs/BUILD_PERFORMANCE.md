# Rust build performance

NovaGPT favors repeatable, safe builds over compiler tricks that make one target fail or change the MCP protocol. The release matrix builds three native targets (Linux x86_64, Linux ARM64 and Windows x86_64), and PR checks run on Linux and Windows. macOS GitHub CI jobs are disabled.

## Current configuration

| Target | Build runner | Build acceleration |
| --- | --- | --- |
| `x86_64-unknown-linux-gnu` | Ubuntu 24.04 x64 | Cargo dependency cache; Clang + LLVM LLD linker when already available |
| `aarch64-unknown-linux-gnu` | Ubuntu 24.04 ARM64 | Separate ARM64 Cargo cache; Clang + LLVM LLD when available |
| `x86_64-pc-windows-msvc` | Windows Server 2025 | Architecture-specific Cargo cache; standard MSVC linker |

The Linux linker choice is **CI only**, conditional on Clang and `ld.lld` being installed; otherwise the build uses the normal linker. No toolchain flags are imposed on developers' machines, and the workflows do not use nightly Rust or require new native dependencies.

The pinned `Swatinem/rust-cache` action includes the Rust toolchain, target/host environment, compiler flags and Cargo manifests in its cache identity. This avoids stale caches from fixed `Cargo.lock`-only keys. The first build after a compiler, profile or dependency change still needs to compile from source; caches help later runs. Cache storage and cross-branch access follow GitHub Actions' normal limits.

## Local development and tests

```bash
cargo check --workspace            # fast feedback without producing final binaries
cargo build -p novagpt-cli          # normal debug executable
cargo test --workspace             # unit and integration tests
```

The development and test profiles keep optimization at Cargo's normal default (`opt-level=0`), but generate only line-table debug information for workspace crates and no debug information for third-party dependencies. This reduces compilation, linking and disk use, while preserving useful panic locations and stack traces. Incremental compilation remains on locally, as Cargo normally configures it for development. In CI, the Rust cache action disables incremental builds because transient workspace incremental artifacts would not be reused by the cache.

If you need to debug variables inside application **and dependency** code, use the explicit full-debug profile:

```bash
cargo build --profile debugging -p novagpt-cli
```

This profile intentionally builds a separate set of artifacts and requires a first-time rebuild. It does not alter release output.

For rapid POSIX source iteration with an owned server and ngrok tunnel, use `python3 script/dev.py --port 8787 --ngrok`. It shares `target/debug` with normal `cargo build` and keeps the previous working server alive while Rust recompiles. Do not run another service on that port.

## Native releases

```bash
cargo build --release --locked -p novagpt-cli --timings
```

The release profile retains `opt-level=3`, the default panic behavior and no extra LTO, but uses `codegen-units=32` instead of Cargo's default 16 to expose more parallel code generation. This is a build-time/runtime trade-off: it can enlarge the binary and slightly reduce peak runtime performance. Revisit it using actual per-platform measurements and runtime benchmarks rather than assuming a universal speedup.

Cargo writes compilation timing reports to `target/cargo-timings/cargo-timing.html`. Release builds upload the corresponding HTML report as a **cargo-timings-<platform>** artifact, while PR checks and release verification upload their own test timing reports. These are separate from release archives and help diagnose individual crate bottlenecks. The report is not distributed to end users.

### Optional faster linker on your own Linux machine

For an installed Clang and LLVM LLD, an individual developer may place the following in **their own** `~/.cargo/config.toml`:

```toml
[target.x86_64-unknown-linux-gnu]
linker = "clang"
rustflags = ["-C", "link-arg=-fuse-ld=lld"]
```

Use the matching `aarch64-unknown-linux-gnu` target on native Linux ARM64. This is optional and should be removed if it conflicts with custom native libraries; compiler flags change Cargo cache keys and cause a one-time rebuild. Windows targets continue to use the platform linker; macOS builds are not part of the GitHub Actions matrix.

## Measuring improvements responsibly

Use `cargo build --timings` to identify slow crates and linking stages, and measure at least two distinct scenarios: a fresh build and a small-source-change rebuild. Compare equal compiler versions, profiles, cache state and machine load. Keep release codegen changes only when a representative build-time benefit outweighs any runtime/binary-size changes.

During implementation, on Linux x64 with an existing target cache, a release build before profile changes took **72.5 seconds**. The first rebuild with the updated release profile took **84.5 seconds**; these numbers are *not comparable*, because the profile change invalidated cached compiled dependencies. GitHub Actions timing reports are needed to measure the actual cold and warm effects for each target. Cross-platform speedups are therefore **not yet verified**.

## References

- [Cargo: Optimizing Build Performance](https://doc.rust-lang.org/cargo/guide/build-performance.html)
- [Cargo: Profiles](https://doc.rust-lang.org/cargo/reference/profiles.html)
- [Cargo: Reporting build timings](https://doc.rust-lang.org/cargo/reference/timings.html)
- [Rust Cache action documentation](https://github.com/Swatinem/rust-cache)
- [GitHub Actions cache behavior](https://docs.github.com/en/actions/reference/workflows-and-actions/dependency-caching)
