#!/bin/bash

# CPU-only
cargo test --release add11_yml
cargo test --release test_erc20
cargo test --release test_erc721
cargo test --release test_global_exit_root
cargo test --release test_log_opcodes
cargo test --release test_selfdestruct
cargo test --release test_simple_transfer
cargo test --release test_two_to_one_block_aggregation
cargo test --release test_withdrawals

# CPU + GPU (CUDA)
cargo test --release --features=cuda add11_yml
cargo test --release --features=cuda test_erc20
cargo test --release --features=cuda test_erc721
cargo test --release --features=cuda test_global_exit_root
cargo test --release --features=cuda test_log_opcodes
cargo test --release --features=cuda test_selfdestruct
cargo test --release --features=cuda test_simple_transfer
cargo test --release --features=cuda test_two_to_one_block_aggregation
cargo test --release --features=cuda test_withdrawals