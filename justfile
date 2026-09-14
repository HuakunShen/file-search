
# Bounded-candidate benchmarks (ignored tests; release profile required —
# debug turso costs ~1.5ms per joined candidate row).
benchmark:
    cargo test --release -p kuntu-index --locked -- --ignored --nocapture
