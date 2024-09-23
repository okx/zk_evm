#!/bin/bash

# CPU-only
TEST_NAMES="add11_yml erc20 erc721 global_exit_root log_opcode selfdestruct simple_transfer two_to_one_block withdrawals"

for TEST_NAME in $TEST_NAMES; do
    cargo test --release --package evm_arithmetization --test $TEST_NAME -- test_$TEST_NAME --exact --show-output
done