use std::collections::HashMap;

use anyhow::Result;
use ethereum_types::{Address, BigEndianHash, H256, U256};
use hex_literal::hex;
use keccak_hash::keccak;
use mpt_trie::nibbles::Nibbles;
use mpt_trie::partial_trie::{HashedPartialTrie, Node, PartialTrie};
use plonky2::field::goldilocks_field::GoldilocksField as F;
use plonky2::hash::hash_types::RichField;
use rand::{thread_rng, Rng};

use crate::cpu::kernel::aggregator::KERNEL;
use crate::cpu::kernel::constants::context_metadata::ContextMetadata::{self, GasLimit};
use crate::cpu::kernel::constants::global_metadata::GlobalMetadata;
use crate::cpu::kernel::constants::INITIAL_RLP_ADDR;
use crate::cpu::kernel::interpreter::Interpreter;
use crate::cpu::kernel::tests::mpt::nibbles_64;
use crate::generation::mpt::{
    load_linked_lists_and_txn_and_receipt_mpts, load_state_mpt, AccountRlp,
};
use crate::generation::TrieInputs;
use crate::memory::segments::Segment;
use crate::util::h2u;
use crate::witness::memory::MemoryAddress;
use crate::witness::operation::CONTEXT_SCALING_FACTOR;

pub(crate) fn initialize_mpts<F: RichField>(
    interpreter: &mut Interpreter<F>,
    trie_inputs: &TrieInputs,
) {
    // Load all MPTs.
    let (mut trie_root_ptrs, state_leaves, storage_leaves, trie_data) =
        load_linked_lists_and_txn_and_receipt_mpts(
            &mut interpreter.generation_state.state_ptrs.accounts,
            &mut interpreter.generation_state.state_ptrs.storage,
            trie_inputs,
        )
        .expect("Invalid MPT data for preinitialization");

    interpreter.generation_state.memory.contexts[0].segments
        [Segment::AccountsLinkedList.unscale()]
    .content = state_leaves;
    interpreter.generation_state.memory.contexts[0].segments
        [Segment::StorageLinkedList.unscale()]
    .content = storage_leaves;
    interpreter.generation_state.memory.contexts[0].segments[Segment::TrieData.unscale()].content =
        trie_data.clone();
    interpreter.generation_state.trie_root_ptrs = trie_root_ptrs.clone();

    if trie_root_ptrs.state_root_ptr.is_none() {
        trie_root_ptrs.state_root_ptr = Some(
            load_state_mpt(
                &trie_inputs.trim(),
                &mut interpreter.generation_state.memory.contexts[0].segments
                    [Segment::TrieData.unscale()]
                .content,
            )
            .expect("Invalid MPT data for preinitialization"),
        );
    }

    let accounts_len = Segment::AccountsLinkedList as usize
        + interpreter.generation_state.memory.contexts[0].segments
            [Segment::AccountsLinkedList.unscale()]
        .content
        .len();
    let storage_len = Segment::StorageLinkedList as usize
        + interpreter.generation_state.memory.contexts[0].segments
            [Segment::StorageLinkedList.unscale()]
        .content
        .len();
    let accounts_len_addr = MemoryAddress {
        context: 0,
        segment: Segment::GlobalMetadata.unscale(),
        virt: GlobalMetadata::AccountsLinkedListNextAvailable.unscale(),
    };
    let storage_len_addr = MemoryAddress {
        context: 0,
        segment: Segment::GlobalMetadata.unscale(),
        virt: GlobalMetadata::StorageLinkedListNextAvailable.unscale(),
    };
    let initial_accounts_len_addr = MemoryAddress {
        context: 0,
        segment: Segment::GlobalMetadata.unscale(),
        virt: GlobalMetadata::InitialAccountsLinkedListLen.unscale(),
    };
    let initial_storage_len_addr = MemoryAddress {
        context: 0,
        segment: Segment::GlobalMetadata.unscale(),
        virt: GlobalMetadata::InitialStorageLinkedListLen.unscale(),
    };
    let trie_data_len_addr = MemoryAddress {
        context: 0,
        segment: Segment::GlobalMetadata.unscale(),
        virt: GlobalMetadata::TrieDataSize.unscale(),
    };
    let trie_data_len = interpreter.generation_state.memory.contexts[0].segments
        [Segment::TrieData.unscale()]
    .content
    .len();
    interpreter.set_memory_multi_addresses(&[
        (accounts_len_addr, accounts_len.into()),
        (storage_len_addr, storage_len.into()),
        (trie_data_len_addr, trie_data_len.into()),
        (initial_accounts_len_addr, accounts_len.into()),
        (initial_storage_len_addr, storage_len.into()),
    ]);

    let state_addr =
        MemoryAddress::new_bundle((GlobalMetadata::StateTrieRoot as usize).into()).unwrap();
    let txn_addr =
        MemoryAddress::new_bundle((GlobalMetadata::TransactionTrieRoot as usize).into()).unwrap();
    let receipts_addr =
        MemoryAddress::new_bundle((GlobalMetadata::ReceiptTrieRoot as usize).into()).unwrap();

    let mut to_set = vec![];
    if let Some(state_root_ptr) = trie_root_ptrs.state_root_ptr {
        to_set.push((state_addr, state_root_ptr.into()));
    }
    to_set.extend([
        (txn_addr, trie_root_ptrs.txn_root_ptr.into()),
        (receipts_addr, trie_root_ptrs.receipt_root_ptr.into()),
    ]);

    interpreter.set_memory_multi_addresses(&to_set);

    for (i, data) in trie_data.iter().enumerate() {
        let trie_addr = MemoryAddress::new(0, Segment::TrieData, i);
        interpreter
            .generation_state
            .memory
            .set(trie_addr, data.unwrap_or_default());
    }
}

// Stolen from `tests/mpt/insert.rs`
// Prepare the interpreter by inserting the account in the state trie.
pub(crate) fn prepare_interpreter<F: RichField>(
    interpreter: &mut Interpreter<F>,
    address: Address,
    account: &AccountRlp,
) -> Result<()> {
    let mpt_insert_state_trie = KERNEL.global_labels["mpt_insert_state_trie"];
    let check_state_trie = KERNEL.global_labels["check_final_state_trie"];
    let mut state_trie: HashedPartialTrie = HashedPartialTrie::from(Node::Empty);
    let trie_inputs = TrieInputs {
        state_trie: HashedPartialTrie::from(Node::Empty),
        transactions_trie: HashedPartialTrie::from(Node::Empty),
        receipts_trie: HashedPartialTrie::from(Node::Empty),
        storage_tries: vec![],
    };

    initialize_mpts(interpreter, &trie_inputs);
    assert_eq!(interpreter.stack(), vec![]);

    let k = nibbles_64(U256::from_big_endian(
        keccak(address.to_fixed_bytes()).as_bytes(),
    ));
    // Next, execute mpt_insert_state_trie.
    interpreter.generation_state.registers.program_counter = mpt_insert_state_trie;
    let trie_data = interpreter.get_trie_data_mut();
    if trie_data.is_empty() {
        // In the assembly we skip over 0, knowing trie_data[0] = 0 by default.
        // Since we don't explicitly set it to 0, we need to do so here.
        trie_data.push(Some(0.into()));
    }
    let value_ptr = trie_data.len();
    trie_data.push(Some(account.nonce));
    trie_data.push(Some(account.balance));
    // In memory, storage_root gets interpreted as a pointer to a storage trie,
    // so we have to ensure the pointer is valid. It's easiest to set it to 0,
    // which works as an empty node, since trie_data[0] = 0 = MPT_TYPE_EMPTY.
    trie_data.push(Some(H256::zero().into_uint()));
    trie_data.push(Some(account.code_hash.into_uint()));
    let trie_data_len = trie_data.len().into();
    interpreter.set_global_metadata_field(GlobalMetadata::TrieDataSize, trie_data_len);
    interpreter
        .push(0xDEADBEEFu32.into())
        .expect("The stack should not overflow");
    interpreter
        .push(value_ptr.into())
        .expect("The stack should not overflow"); // value_ptr
    interpreter
        .push(k.try_into().unwrap())
        .expect("The stack should not overflow"); // key

    interpreter.run()?;

    assert_eq!(
        interpreter.stack().len(),
        0,
        "Expected empty stack after insert, found {:?}",
        interpreter.stack()
    );

    // Now, set the payload.
    interpreter
        .push(0xDEADBEEFu32.into())
        .expect("The stack should not overflow");
    interpreter
        .push((Segment::StorageLinkedList as usize + 5).into())
        .expect("The stack should not overflow");
    interpreter
        .push(interpreter.get_global_metadata_field(GlobalMetadata::StateTrieRoot))
        .unwrap();
    interpreter
        .push((Segment::AccountsLinkedList as usize + 4).into())
        .expect("The stack should not overflow");

    // Now, set the payload.
    interpreter.generation_state.registers.program_counter =
        KERNEL.global_labels["insert_all_initial_accounts"];

    interpreter.run()?;

    assert_eq!(interpreter.stack_len(), 1);

    let state_root = interpreter.pop().expect("The stack should not be empty");
    interpreter.set_global_metadata_field(GlobalMetadata::StateTrieRoot, state_root);

    // Now, execute `mpt_hash_state_trie`.
    state_trie.insert(k, rlp::encode(account).to_vec())?;
    let expected_state_trie_hash = state_trie.hash();
    interpreter.set_global_metadata_field(
        GlobalMetadata::StateTrieRootDigestAfter,
        h2u(expected_state_trie_hash),
    );

    interpreter.generation_state.registers.program_counter = check_state_trie;
    interpreter
        .halt_offsets
        .push(KERNEL.global_labels["check_txn_trie"]);
    interpreter
        .push(0xDEADBEEFu32.into())
        .expect("The stack should not overflow");
    interpreter
        .push(interpreter.get_global_metadata_field(GlobalMetadata::TrieDataSize)) // Initial trie data segment size, unused.
        .expect("The stack should not overflow");
    interpreter.run()?;

    assert_eq!(
        interpreter.stack().len(),
        2,
        "Expected 2 items on stack after hashing, found {:?}",
        interpreter.stack()
    );

    Ok(())
}

// Test account with a given code hash.
fn test_account(code: &[u8]) -> AccountRlp {
    AccountRlp {
        nonce: U256::from(1111),
        balance: U256::from(2222),
        storage_root: HashedPartialTrie::from(Node::Empty).hash(),
        code_hash: keccak(code),
    }
}

fn random_code() -> Vec<u8> {
    let mut rng = thread_rng();
    let num_bytes = rng.gen_range(0..1000);
    (0..num_bytes).map(|_| rng.gen()).collect()
}

#[test]
fn test_extcodesize() -> Result<()> {
    let code = random_code();
    let account = test_account(&code);

    let mut interpreter: Interpreter<F> = Interpreter::new(0, vec![], None);
    let address: Address = thread_rng().gen();
    // Prepare the interpreter by inserting the account in the state trie.
    prepare_interpreter(&mut interpreter, address, &account)?;

    let extcodesize = KERNEL.global_labels["extcodesize"];

    // Test `extcodesize`
    interpreter.generation_state.registers.program_counter = extcodesize;
    interpreter.pop().expect("The stack should not be empty");
    interpreter.pop().expect("The stack should not be empty");
    assert!(interpreter.stack().is_empty());
    interpreter
        .push(0xDEADBEEFu32.into())
        .expect("The stack should not overflow");
    interpreter
        .push(U256::from_big_endian(address.as_bytes()))
        .expect("The stack should not overflow");
    interpreter.generation_state.inputs.contract_code =
        HashMap::from([(keccak(&code), code.clone())]);
    interpreter.run()?;

    assert_eq!(
        interpreter.stack(),
        vec![U256::one() << CONTEXT_SCALING_FACTOR, code.len().into()]
    );

    Ok(())
}

#[test]
fn test_extcodecopy() -> Result<()> {
    let code = random_code();
    let account = test_account(&code);

    let mut interpreter: Interpreter<F> = Interpreter::new(0, vec![], None);
    let address: Address = thread_rng().gen();
    // Prepare the interpreter by inserting the account in the state trie.
    prepare_interpreter(&mut interpreter, address, &account)?;

    let context = interpreter.context();
    interpreter.generation_state.memory.contexts[context].segments
        [Segment::ContextMetadata.unscale()]
    .set(GasLimit.unscale(), U256::from(1000000000000u64));

    // Pre-initialize the accessed addresses list.
    let init_accessed_addresses = KERNEL.global_labels["init_access_lists"];
    interpreter.generation_state.registers.program_counter = init_accessed_addresses;
    interpreter
        .push(0xdeadbeefu32.into())
        .expect("The stack should not overflow");
    interpreter.run()?;

    let extcodecopy = KERNEL.global_labels["sys_extcodecopy"];

    // Put random data in main memory and the `KernelAccountCode` segment for
    // realism.
    let mut rng = thread_rng();
    for i in 0..2000 {
        interpreter.generation_state.memory.contexts[context].segments
            [Segment::MainMemory.unscale()]
        .set(i, U256::from(rng.gen::<u8>()));
        interpreter.generation_state.memory.contexts[context].segments
            [Segment::KernelAccountCode.unscale()]
        .set(i, U256::from(rng.gen::<u8>()));
    }

    // Random inputs
    let dest_offset = rng.gen_range(0..3000);
    let offset = rng.gen_range(0..1500);
    let size = rng.gen_range(0..1500);

    // Test `extcodecopy`
    interpreter.generation_state.registers.program_counter = extcodecopy;
    interpreter.pop().expect("The stack should not be empty");
    interpreter.pop().expect("The stack should not be empty");
    assert!(interpreter.stack().is_empty());
    interpreter
        .push(size.into())
        .expect("The stack should not overflow");
    interpreter
        .push(offset.into())
        .expect("The stack should not overflow");
    interpreter
        .push(dest_offset.into())
        .expect("The stack should not overflow");
    interpreter
        .push(U256::from_big_endian(address.as_bytes()))
        .expect("The stack should not overflow");
    interpreter
        .push((0xDEADBEEFu64 + (1 << 32)).into())
        .expect("The stack should not overflow"); // kexit_info
    interpreter.generation_state.inputs.contract_code =
        HashMap::from([(keccak(&code), code.clone())]);
    interpreter.run()?;

    assert!(interpreter.stack().is_empty());
    // Check that the code was correctly copied to memory.
    for i in 0..size {
        let memory = interpreter.generation_state.memory.contexts[context].segments
            [Segment::MainMemory.unscale()]
        .get(dest_offset + i);
        assert_eq!(
            memory,
            code.get(offset + i).copied().unwrap_or_default().into()
        );
    }

    Ok(())
}

/// Prepare the interpreter for storage tests by inserting all necessary
/// accounts in the state trie, adding the code we want to context 1 and
/// switching the context.
fn prepare_interpreter_all_accounts<F: RichField>(
    interpreter: &mut Interpreter<F>,
    trie_inputs: TrieInputs,
    addr: [u8; 20],
    code: &[u8],
) -> Result<()> {
    // Load all MPTs.
    initialize_mpts(interpreter, &trie_inputs);
    assert_eq!(interpreter.stack(), vec![]);

    // Copy the initial account and storage pointers
    interpreter
        .push(0xDEADBEEFu32.into())
        .expect("The stack should not overflow");
    interpreter.generation_state.registers.program_counter =
        KERNEL.global_labels["store_initial_accounts"];
    interpreter.run()?;
    interpreter
        .push(0xDEADBEEFu32.into())
        .expect("The stack should not overflow");
    interpreter.generation_state.registers.program_counter =
        KERNEL.global_labels["store_initial_slots"];
    interpreter.run()?;

    // Now, set the payload.
    interpreter
        .push(0xDEADBEEFu32.into())
        .expect("The stack should not overflow");
    interpreter
        .push((Segment::StorageLinkedList as usize + 5).into())
        .expect("The stack should not overflow");
    interpreter
        .push(interpreter.get_global_metadata_field(GlobalMetadata::StateTrieRoot))
        .unwrap();
    interpreter
        .push((Segment::AccountsLinkedList as usize + 4).into())
        .expect("The stack should not overflow");

    // Now, set the payload.
    interpreter.generation_state.registers.program_counter =
        KERNEL.global_labels["insert_all_initial_accounts"];

    interpreter.run()?;

    assert_eq!(interpreter.stack_len(), 1);

    let state_root = interpreter.pop().expect("The stack should not be empty");
    interpreter.set_global_metadata_field(GlobalMetadata::StateTrieRoot, state_root);

    // Switch context and initialize memory with the data we need for the tests.
    interpreter.generation_state.registers.program_counter = 0;
    interpreter.set_code(1, code.to_vec());
    interpreter.set_context_metadata_field(
        1,
        ContextMetadata::Address,
        U256::from_big_endian(&addr),
    );
    interpreter.set_context_metadata_field(1, ContextMetadata::GasLimit, 100_000.into());
    interpreter.set_context(1);
    interpreter.set_is_kernel(false);
    interpreter.set_context_metadata_field(
        1,
        ContextMetadata::ParentProgramCounter,
        0xdeadbeefu32.into(),
    );
    interpreter.set_context_metadata_field(
        1,
        ContextMetadata::ParentContext,
        U256::one() << CONTEXT_SCALING_FACTOR, // ctx = 1
    );

    Ok(())
}

/// Tests an SSTORE within a code similar to the contract code in add11_yml.
#[test]
fn sstore() -> Result<()> {
    // We take the same `to` account as in add11_yml.
    let addr = hex!("095e7baea6a6c7c4c2dfeb977efac326af552d87");

    let addr_hashed = keccak(addr);

    let addr_nibbles = Nibbles::from_bytes_be(addr_hashed.as_bytes()).unwrap();

    let code = [0x60, 0x01, 0x60, 0x01, 0x01, 0x60, 0x00, 0x55, 0x00];
    let code_hash = keccak(code);

    let account_before = AccountRlp {
        balance: 0x0de0b6b3a7640000u64.into(),
        code_hash,
        ..AccountRlp::default()
    };

    let mut state_trie_before = HashedPartialTrie::from(Node::Empty);

    state_trie_before.insert(addr_nibbles, rlp::encode(&account_before).to_vec())?;

    let trie_inputs = TrieInputs {
        state_trie: state_trie_before.clone(),
        transactions_trie: Node::Empty.into(),
        receipts_trie: Node::Empty.into(),
        storage_tries: vec![(addr_hashed, Node::Empty.into())],
    };

    let initial_stack = vec![];
    let mut interpreter: Interpreter<F> = Interpreter::new(0, initial_stack, None);

    // Pre-initialize the accessed addresses list.
    let init_accessed_addresses = KERNEL.global_labels["init_access_lists"];
    interpreter.generation_state.registers.program_counter = init_accessed_addresses;
    interpreter
        .push(0xdeadbeefu32.into())
        .expect("The stack should not overflow");
    interpreter.run()?;

    // Prepare the interpreter by inserting the account in the state trie.
    prepare_interpreter_all_accounts(&mut interpreter, trie_inputs, addr, &code)?;

    interpreter.run()?;

    // The first two elements in the stack are `success` and `leftover_gas`,
    // returned by the `sys_stop` opcode.
    interpreter.pop().expect("Stack should not be empty");
    interpreter.pop().expect("Stack should not be empty");

    // The code should have added an element to the storage of `to_account`. We run
    // `mpt_hash_state_trie` to check that.
    let account_after = AccountRlp {
        balance: 0x0de0b6b3a7640000u64.into(),
        code_hash,
        storage_root: HashedPartialTrie::from(Node::Leaf {
            nibbles: Nibbles::from_h256_be(keccak([0u8; 32])),
            value: vec![2],
        })
        .hash(),
        ..AccountRlp::default()
    };

    let mut expected_state_trie_after = HashedPartialTrie::from(Node::Empty);
    expected_state_trie_after.insert(addr_nibbles, rlp::encode(&account_after).to_vec())?;

    let expected_state_trie_hash = expected_state_trie_after.hash();

    interpreter.set_global_metadata_field(
        GlobalMetadata::StateTrieRootDigestAfter,
        h2u(expected_state_trie_hash),
    );

    // Now, execute `mpt_hash_state_trie` and check that the hash is correct.
    let mpt_hash_state_trie = KERNEL.global_labels["check_final_state_trie"];
    interpreter.generation_state.registers.program_counter = mpt_hash_state_trie;
    interpreter.set_is_kernel(true);
    interpreter.set_context(0);
    interpreter
        .push(0xDEADBEEFu32.into())
        .expect("The stack should not overflow");
    interpreter
        .push(1.into()) // Initial length of the trie data segment, unused.
        .expect("The stack should not overflow");
    interpreter.run()?;

    Ok(())
}

/// Tests an SLOAD within a code similar to the contract code in add11_yml.
#[test]
fn sload() -> Result<()> {
    let addr = hex!("095e7baea6a6c7c4c2dfeb977efac326af552d87");

    let addr_hashed = keccak(addr);

    let addr_nibbles = Nibbles::from_bytes_be(addr_hashed.as_bytes()).unwrap();

    // This code is similar to the one in add11_yml's contract, but we pop the added
    // value and carry out an SLOAD instead of an SSTORE. We also add a PUSH at
    // the end.
    let code = [
        0x60, 0x01, 0x60, 0x01, 0x01, 0x50, 0x60, 0x00, 0x54, 0x60, 0x03, 0x00,
    ];
    let code_hash = keccak(code);

    let account_before = AccountRlp {
        balance: 0x0de0b6b3a7640000u64.into(),
        code_hash,
        ..AccountRlp::default()
    };

    let mut state_trie_before = HashedPartialTrie::from(Node::Empty);

    state_trie_before.insert(addr_nibbles, rlp::encode(&account_before).to_vec())?;

    let trie_inputs = TrieInputs {
        state_trie: state_trie_before.clone(),
        transactions_trie: Node::Empty.into(),
        receipts_trie: Node::Empty.into(),
        storage_tries: vec![(addr_hashed, Node::Empty.into())],
    };

    let initial_stack = vec![];
    let mut interpreter: Interpreter<F> = Interpreter::new(0, initial_stack, None);

    // Pre-initialize the accessed addresses list.
    let init_accessed_addresses = KERNEL.global_labels["init_access_lists"];
    interpreter.generation_state.registers.program_counter = init_accessed_addresses;
    interpreter
        .push(0xdeadbeefu32.into())
        .expect("The stack should not overflow");
    interpreter.run()?;

    // Prepare the interpreter by inserting the account in the state trie.
    prepare_interpreter_all_accounts(&mut interpreter, trie_inputs, addr, &code)?;
    interpreter.run()?;

    // The first two elements in the stack are `success` and `leftover_gas`,
    // returned by the `sys_stop` opcode.
    interpreter
        .pop()
        .expect("The stack length should not be empty.");
    interpreter
        .pop()
        .expect("The stack length should not be empty.");

    // The SLOAD in the provided code should return 0, since
    // the storage trie is empty. The last step in the code
    // pushes the value 3.
    assert_eq!(interpreter.stack(), vec![0x0.into(), 0x3.into()]);
    interpreter
        .pop()
        .expect("The stack length should not be empty.");
    interpreter
        .pop()
        .expect("The stack length should not be empty.");
    // Now, execute `mpt_hash_state_trie`. We check that the state trie has not
    // changed.
    let mpt_hash_state_trie = KERNEL.global_labels["mpt_hash_state_trie"];
    interpreter.generation_state.registers.program_counter = mpt_hash_state_trie;
    interpreter.set_is_kernel(true);
    interpreter.set_context(0);
    interpreter
        .push(0xDEADBEEFu32.into())
        .expect("The stack should not overflow.");
    interpreter
        .push(interpreter.get_global_metadata_field(GlobalMetadata::TrieDataSize)) // Initial length of the trie data segment, unused.
        .expect("The stack should not overflow.");
    interpreter
        .push(INITIAL_RLP_ADDR.1.into()) // rlp_start
        .expect("The stack should not overflow.");
    interpreter.run()?;

    assert_eq!(
        interpreter.stack().len(),
        2,
        "Expected 2 items on stack after hashing, found {:?}",
        interpreter.stack()
    );

    let hash = H256::from_uint(&interpreter.stack()[1]);

    let expected_state_trie_hash = state_trie_before.hash();
    assert_eq!(hash, expected_state_trie_hash);
    Ok(())
}
