use crate::spv::headercache::HeaderCache;
use crate::Action;
use bitcoin::hashes::Hash;
use bitcoin::Network::Testnet as bitcoin_network;
use failure::bail;
use lazy_static::lazy_static;
use nomic_bitcoin::{bitcoin, EnrichedHeader};
use nomic_primitives::transaction::Transaction;
use nomic_primitives::transaction::{
    DepositTransaction, HeaderTransaction, Sighash, SignatureTransaction, TransferTransaction,
    WithdrawalTransaction, WorkProofTransaction,
};
use nomic_primitives::{Account, Address, Signature, Withdrawal};
use nomic_primitives::{Error, Result};
use nomic_signatory_set::{Signatory, SignatorySet, SignatorySetSnapshot};
use nomic_work::work;
use orga::abci::messages::Header;
use orga::Store;
use orga::{
    collections::{Deque, Map, Set},
    state, Decode, Encode, Value, WrapStore,
};
use secp256k1::{Secp256k1, VerifyOnly};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

const MIN_WORK: u64 = 1 << 20;
pub const SIGNATORY_CHANGE_INTERVAL: u64 = 60 * 60 * 24 * 7;

lazy_static! {
    static ref SECP: Secp256k1<VerifyOnly> = Secp256k1::verification_only();
}

#[derive(Encode, Decode)]
pub struct Utxo {
    outpoint: nomic_bitcoin::Outpoint,
    signatory_set_index: u64,
    data: Vec<u8>,
}

pub struct Checkpoint {
    utxos: Vec<Utxo>,
    withdrawals: Vec<Withdrawal>,
    signatory_set: SignatorySetSnapshot,
}

impl Encode for Checkpoint {
    fn encode_into<W: std::io::Write>(&self, mut dest: &mut W) -> Result<()> {
        (self.utxos.len() as u32).encode_into(&mut dest)?;
        self.utxos.encode_into(&mut dest)?;

        (self.withdrawals.len() as u32).encode_into(&mut dest)?;
        self.withdrawals.encode_into(&mut dest)?;

        self.signatory_set.encode_into(dest)
    }

    fn encoding_length(&self) -> Result<usize> {
        Ok(4 + self.utxos.encoding_length()?
            + 4
            + self.withdrawals.encoding_length()?
            + self.signatory_set.encoding_length()?)
    }
}

impl Decode for Checkpoint {
    fn decode<R: std::io::Read>(mut input: R) -> Result<Self> {
        let utxo_len: u32 = Decode::decode(&mut input)?;
        let mut utxos = Vec::with_capacity(utxo_len as usize);
        for _ in 0..utxo_len {
            utxos.push(Decode::decode(&mut input)?);
        }

        let withdrawal_len: u32 = Decode::decode(&mut input)?;
        let mut withdrawals = Vec::with_capacity(withdrawal_len as usize);
        for _ in 0..withdrawal_len {
            withdrawals.push(Decode::decode(&mut input)?);
        }

        Ok(Checkpoint {
            utxos,
            withdrawals,
            signatory_set: Decode::decode(input)?,
        })
    }
}

#[state]
pub struct State {
    pub accounts: Map<Address, Account>,
    pub redeemed_work_hashes: Set<[u8; 32]>,

    // Peg state
    pub signatory_sets: Deque<SignatorySetSnapshot>,
    pub processed_deposit_txids: Set<[u8; 32]>,
    pub pending_withdrawals: Deque<Withdrawal>,
    pub utxos: Deque<Utxo>,
    pub finalized_checkpoint: Value<Checkpoint>,
    pub active_checkpoint: ActiveCheckpoint,
}

impl<S: Store> State<S> {
    pub fn current_signatory_set(&self) -> Result<SignatorySetSnapshot> {
        Ok(self.signatory_sets.back()?.unwrap())
    }
}

#[state]
pub struct ActiveCheckpoint {
    pub is_active: Value<bool>,
    pub signatures: Map<u16, Signature>,
    pub signed_voting_power: Value<u64>,
    pub signatory_set: Value<SignatorySetSnapshot>,
    pub utxos: Deque<Withdrawal>,
    pub withdrawals: Deque<Withdrawal>,
}

/// Main entrypoint to the core bitcoin peg state machine.
///
/// This function implements the conventions set by Orga, though this may change
/// as our core framework design settles.
pub fn run<S: Store>(
    mut store: S,
    action: Action,
    validators: &mut BTreeMap<Vec<u8>, u64>,
) -> Result<()> {
    let mut state = State::wrap_store(&mut store)?;
    match action {
        Action::BeginBlock(header) => handle_begin_block(&mut state, validators, header),
        Action::Transaction(transaction) => match transaction {
            Transaction::WorkProof(tx) => handle_work_proof_tx(&mut state, validators, tx),
            Transaction::Header(tx) => handle_header_tx(&mut store, tx),
            Transaction::Deposit(tx) => handle_deposit_tx(&mut store, tx),
            Transaction::Transfer(tx) => handle_transfer_tx(&mut state, tx),
            Transaction::Withdrawal(tx) => handle_withdrawal_tx(&mut state, tx),
            Transaction::Signature(tx) => handle_signature_tx(&mut state, tx),
        },
    }
}

fn handle_begin_block<S: Store>(
    state: &mut State<S>,
    validators: &BTreeMap<Vec<u8>, u64>,
    header: Header,
) -> Result<()> {
    match state.signatory_sets.back()? {
        None => {
            // init signatories at start of chain
            let signatories = SignatorySetSnapshot {
                time: header.get_time().get_seconds() as u64,
                signatories: signatories_from_validators(validators)?,
            };
            state.signatory_sets.push_back(signatories)?;
        }
        Some(signatories) => {
            // check if signatories should be updated
            let now = header.get_time().get_seconds() as u64;
            let elapsed = now - signatories.time;
            if elapsed >= SIGNATORY_CHANGE_INTERVAL {
                let new_signatories = SignatorySetSnapshot {
                    time: now,
                    signatories: signatories_from_validators(validators)?,
                };
                state.signatory_sets.push_back(new_signatories)?;
            }
        }
    }

    Ok(())
}

fn handle_work_proof_tx<S: Store>(
    state: &mut State<S>,
    validators: &mut BTreeMap<Vec<u8>, u64>,
    tx: WorkProofTransaction,
) -> Result<()> {
    let mut hasher = Sha256::new();
    hasher.input(&tx.public_key);
    let nonce_bytes = tx.nonce.to_be_bytes();
    hasher.input(&nonce_bytes);
    let hash: [u8; 32] = hasher.result().into();
    let work_proof_value = work(&hash);

    if work_proof_value < MIN_WORK {
        bail!("Proof has less than minimum work value")
    }

    // Make sure this proof hasn't been redeemed yet
    if state.redeemed_work_hashes.contains(hash)? {
        bail!("Work proof has already been redeemed")
    }

    // Grant voting power
    let current_voting_power = *validators.get(&tx.public_key).unwrap_or(&0);

    validators.insert(tx.public_key, current_voting_power + work_proof_value);
    // Write the redeemed hash to the store so it can't be replayed
    state.redeemed_work_hashes.insert(hash)?;

    Ok(())
}

fn handle_header_tx<S: Store>(mut store: S, tx: HeaderTransaction) -> Result<()> {
    let mut header_cache = HeaderCache::new(bitcoin_network, &mut store);
    for header in tx.block_headers {
        header_cache.add_header(&header)?;
    }
    Ok(())
}

fn handle_deposit_tx<S: Store>(
    mut store: S,
    deposit_transaction: DepositTransaction,
) -> Result<()> {
    let state = State::wrap_store(&mut store)?;
    // Hash transaction and check for duplicate
    let txid = deposit_transaction.tx.txid();
    if state
        .processed_deposit_txids
        .contains(txid.as_hash().into_inner())?
    {
        bail!("Transaction was already processed");
    }

    // Fetch merkle root for this block by its height
    let mut header_cache = HeaderCache::new(bitcoin_network, &mut store);
    let tx_height = deposit_transaction.height;
    let header = header_cache.get_header_for_height(tx_height)?;

    let header_merkle_root = match header {
        Some(header) => header.stored.header.merkle_root,
        None => bail!("Merkle root not found for deposit transaction"),
    };

    // Verify proof against the merkle root
    let proof = deposit_transaction.proof;
    let mut txids = vec![txid];
    let mut indexes = vec![deposit_transaction.block_index];
    let proof_merkle_root = proof
        .extract_matches(&mut txids, &mut indexes)
        .map_err(Error::from)?;

    let proof_matches_chain_merkle_root = proof_merkle_root == header_merkle_root;
    if !proof_matches_chain_merkle_root {
        bail!("Proof merkle root does not match chain");
    }

    let mut state = State::wrap_store(&mut store)?;
    // Ensure tx contains deposit outputs
    let mut recipients = deposit_transaction.recipients.iter().peekable();
    let mut contains_deposit_outputs = false;
    for (i, txout) in deposit_transaction.tx.output.iter().enumerate() {
        let recipient = match recipients.peek() {
            Some(recipient) => recipient,
            None => bail!("Consumed all recipients"),
        };
        if recipient.len() != 33 {
            bail!("Recipient must be 33 bytes");
        }
        for (signatory_set_index, signatory_set) in state.signatory_sets.iter().enumerate() {
            let signatory_set = signatory_set?;
            let expected_script =
                nomic_signatory_set::output_script(&signatory_set.signatories, recipient.to_vec());
            if txout.script_pubkey != expected_script {
                continue;
            }

            // mint coins
            let depositor_address = unsafe_slice_to_address(recipient.as_slice());
            let mut depositor_account = state.accounts.get(depositor_address)?.unwrap_or_default();
            depositor_account.balance += txout.value;
            state
                .accounts
                .insert(depositor_address, depositor_account)?;

            // Add UTXO to state
            let utxo = Utxo {
                outpoint: bitcoin::OutPoint {
                    txid: deposit_transaction.tx.txid(),
                    vout: i as u32,
                }
                .into(),
                signatory_set_index: signatory_set_index as u64,
                data: recipient.to_vec(),
            };
            state.utxos.push_back(utxo)?;

            contains_deposit_outputs = true;
            break;
        }
    }
    if !contains_deposit_outputs {
        bail!("Transaction does not contain any deposit outputs");
    }

    // Deposit is valid, mark transaction as processed
    state
        .processed_deposit_txids
        .insert(txid.as_hash().into_inner())?;
    Ok(())
}

fn unsafe_slice_to_address(slice: &[u8]) -> Address {
    // warning: only call this with a slice of length 32
    let mut buf: Address = [0; 33];
    buf.copy_from_slice(slice);
    buf
}

fn handle_transfer_tx<S: Store>(state: &mut State<S>, tx: TransferTransaction) -> Result<()> {
    if tx.from == tx.to {
        bail!("Account cannot send to itself");
    }
    if tx.fee_amount < 1000 {
        bail!("Transaction fee is too small");
    }
    if tx.from.len() != 33 {
        bail!("Invalid sender address");
    }
    if tx.to.len() != 33 {
        bail!("Invalid recipient address");
    }
    // Retrieve sender account from store
    let maybe_sender_account = state.accounts.get(unsafe_slice_to_address(&tx.from[..]))?;
    let mut sender_account = match maybe_sender_account {
        Some(sender_account) => sender_account,
        None => bail!("Account does not exist"),
    };
    // Check that the sender account has enough coins
    if sender_account.balance < (tx.amount + tx.fee_amount) {
        bail!("Insufficient balance in sender account");
    }
    // Verify the nonce
    if tx.nonce != sender_account.nonce {
        bail!("Invalid account nonce for transaction");
    }
    // Verify the signature
    if !tx.verify_signature(&SECP)? {
        bail!("Invalid signature");
    }
    // Increment sender's nonce
    sender_account.nonce += 1;
    // Subtract coins from sender
    sender_account.balance -= tx.amount + tx.fee_amount;
    // Fetch (and maybe create) recipient account
    let mut recipient_account = state
        .accounts
        .get(unsafe_slice_to_address(&tx.to[..]))?
        .unwrap_or_default();
    // Add coins to recipient
    recipient_account.balance += tx.amount;
    // Save updated accounts to store
    state
        .accounts
        .insert(unsafe_slice_to_address(&tx.from[..]), sender_account)?;
    state
        .accounts
        .insert(unsafe_slice_to_address(&tx.to[..]), recipient_account)?;
    Ok(())
}

fn handle_withdrawal_tx<S: Store>(state: &mut State<S>, tx: WithdrawalTransaction) -> Result<()> {
    if tx.from.len() != 33 {
        bail!("Invalid sender address");
    }
    let maybe_sender_account = state.accounts.get(unsafe_slice_to_address(&tx.from[..]))?;
    let mut sender_account = match maybe_sender_account {
        Some(sender_account) => sender_account,
        None => bail!("Account does not exist"),
    };

    if sender_account.balance < tx.amount {
        bail!("Insufficient balance in sender account");
    }

    // Verify the nonce
    if tx.nonce != sender_account.nonce {
        bail!("Invalid account nonce for withdrawal transaction");
    }
    // Verify signature
    if !tx.verify_signature(&SECP)? {
        bail!("Invalid signature");
    }

    sender_account.nonce += 1;

    sender_account.balance -= tx.amount;
    state
        .accounts
        .insert(unsafe_slice_to_address(&tx.from[..]), sender_account)?;

    use nomic_bitcoin::Script;
    // Push withdrawal to pending withdrawals deque
    let withdrawal = Withdrawal {
        value: tx.amount,
        script: Script(tx.to),
    };
    Ok(state.pending_withdrawals.push_back(withdrawal)?)
}
fn handle_signature_tx<S: Store>(state: &mut State<S>, tx: SignatureTransaction) -> Result<()> {
    bail!("Unimplemented transaction type")
}
fn signatories_from_validators(validators: &BTreeMap<Vec<u8>, u64>) -> Result<SignatorySet> {
    let mut signatories = SignatorySet::new();
    for (key_bytes, voting_power) in validators.iter() {
        let key = bitcoin::PublicKey::from_slice(key_bytes.as_slice())?;
        signatories.set(Signatory::new(key, *voting_power));
    }
    Ok(signatories)
}

// TODO: this should be Action::InitChain
/// Called once at genesis to write some data to the store.
pub fn initialize<S: Store>(mut store: S) -> Result<()> {
    // TODO: this should be an action
    let checkpoint = get_checkpoint_header();
    let mut header_cache = HeaderCache::new(bitcoin_network, &mut store);

    header_cache
        .add_header_raw(checkpoint.header, checkpoint.height)
        .map_err(|e| e.into())
}

fn get_checkpoint_header() -> EnrichedHeader {
    let encoded_checkpoint = include_bytes!("../../../config/header.json");
    let checkpoint: EnrichedHeader = serde_json::from_slice(&encoded_checkpoint[..])
        .expect("Failed to deserialize checkpoint header");

    checkpoint
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Action;
    use bitcoin::consensus::encode as bitcoin_encode;
    use bitcoin::util::hash::bitcoin_merkle_root;
    use bitcoin::util::merkleblock::PartialMerkleTree;
    use bitcoin::Network::Testnet as bitcoin_network;
    use lazy_static::lazy_static;
    use nomic_primitives::Account;
    use nomic_signatory_set::{Signatory, SignatorySet, SignatorySetSnapshot};
    use orga::{abci::messages::Header as TendermintHeader, MapStore, WrapStore};
    use protobuf::well_known_types::Timestamp;
    use secp256k1::{Secp256k1, SignOnly};
    use std::collections::{BTreeMap, HashSet};

    lazy_static! {
        static ref SECP: Secp256k1<SignOnly> = Secp256k1::signing_only();
    }

    fn mock_validator_set() -> BTreeMap<Vec<u8>, u64> {
        let mut vals = BTreeMap::new();
        vals.insert(
            vec![
                3, 148, 217, 3, 10, 128, 64, 14, 129, 125, 33, 213, 163, 104, 0, 227, 122, 136, 27,
                45, 207, 44, 64, 24, 35, 166, 166, 118, 25, 12, 200, 183, 98,
            ],
            100,
        );
        vals
    }

    struct MockNet {
        store: MapStore,
        validators: BTreeMap<Vec<u8>, u64>,
        btc_block: bitcoin::Block,
    }

    impl MockNet {
        fn new() -> Self {
            let tx = build_tx(vec![build_txout(100_000_000, vec![].into())]);
            let block = build_block(vec![tx.clone()]);
            MockNet::with_btc_block(block)
        }

        fn with_btc_block(initial_block: bitcoin::Block) -> Self {
            let mut net = MockNet {
                store: Default::default(),
                validators: mock_validator_set(),
                btc_block: initial_block.clone(),
            };
            net.spv()
                .add_header_raw(initial_block.header, 0)
                .expect("failed to create mock net");

            // initial beginblock
            let mut header: TendermintHeader = Default::default();
            let mut timestamp = Timestamp::new();
            timestamp.set_seconds(123);
            header.set_time(timestamp);
            let action = Action::BeginBlock(header);
            run(&mut net.store, action, &mut net.validators).unwrap();

            net
        }

        fn spv(&mut self) -> HeaderCache {
            HeaderCache::new(bitcoin::Network::Regtest, &mut self.store)
        }

        fn create_btc_proof(
            &self,
        ) -> (
            bitcoin::Transaction,
            bitcoin::util::merkleblock::PartialMerkleTree,
        ) {
            let tx = self.btc_block.txdata[0].clone();
            let mut txids = HashSet::new();
            txids.insert(tx.txid());
            (
                tx,
                bitcoin::MerkleBlock::from_block(&self.btc_block, &txids).txn,
            )
        }
    }

    fn build_txout(value: u64, script_pubkey: bitcoin::Script) -> bitcoin::TxOut {
        bitcoin::TxOut {
            value,
            script_pubkey,
        }
    }

    fn build_tx(outputs: Vec<bitcoin::TxOut>) -> bitcoin::Transaction {
        bitcoin::Transaction {
            version: 2,
            lock_time: 0,
            input: vec![],
            output: outputs,
        }
    }

    fn build_block(txs: Vec<bitcoin::Transaction>) -> bitcoin::Block {
        let hashes = txs.iter().map(|tx| tx.txid().as_hash());
        let merkle_root = bitcoin_merkle_root(hashes).into();

        let header = bitcoin::BlockHeader {
            version: 1,
            prev_blockhash: Default::default(),
            merkle_root,
            time: 1,
            bits: 0x207fffff,
            nonce: 0,
        };

        bitcoin::Block {
            header,
            txdata: txs,
        }
    }

    fn invalidate_proof(proof: PartialMerkleTree) -> PartialMerkleTree {
        let mut proof_bytes = bitcoin_encode::serialize(&proof);
        proof_bytes[10] ^= 1;
        bitcoin_encode::deserialize(proof_bytes.as_slice()).unwrap()
    }

    fn create_keypair(byte: u8) -> (secp256k1::SecretKey, secp256k1::PublicKey) {
        let privkey = secp256k1::SecretKey::from_slice(&[byte; 32]).unwrap();
        let pubkey = secp256k1::PublicKey::from_secret_key(&SECP, &privkey);
        (privkey, pubkey)
    }

    fn sign<S: Sighash>(tx: &mut S, privkey: secp256k1::SecretKey) -> Vec<u8> {
        let message = secp256k1::Message::from_slice(tx.sighash().unwrap().as_slice()).unwrap();
        let signature = SECP.sign(&message, &privkey);
        signature.serialize_compact().to_vec()
    }

    #[test]
    fn init() {
        let mut store = MapStore::new();
        let chkpt = get_checkpoint_header();
        initialize(&mut store).unwrap();

        let mut header_cache = HeaderCache::new(bitcoin_network, &mut store);
        let header = header_cache
            .get_header_for_height(chkpt.height)
            .unwrap()
            .unwrap();
        assert_eq!(header.stored.header, chkpt.header);
    }

    #[test]
    fn begin_block() {
        let mut net = MockNet::new();
        let state = State::wrap_store(&mut net.store).unwrap();

        // initial signatories
        let mut expected_signatories = SignatorySet::new();
        expected_signatories.set(Signatory {
            pubkey: bitcoin::PublicKey::from_slice(&[
                3, 148, 217, 3, 10, 128, 64, 14, 129, 125, 33, 213, 163, 104, 0, 227, 122, 136, 27,
                45, 207, 44, 64, 24, 35, 166, 166, 118, 25, 12, 200, 183, 98,
            ])
            .unwrap(),
            voting_power: 100,
        });
        assert_eq!(
            state.current_signatory_set().unwrap(),
            SignatorySetSnapshot {
                time: 123,
                signatories: expected_signatories
            }
        );

        // changed validator set, should be same sig set
        net.validators.insert(
            vec![
                2, 120, 15, 192, 99, 177, 43, 235, 23, 134, 193, 123, 205, 196, 253, 121, 49, 80,
                163, 93, 230, 224, 193, 88, 89, 18, 15, 145, 105, 217, 229, 114, 148,
            ],
            555,
        );
        let mut header: TendermintHeader = Default::default();
        let mut timestamp = Timestamp::new();
        timestamp.set_seconds(456);
        header.set_time(timestamp);
        let action = Action::BeginBlock(header);
        run(&mut net.store, action, &mut net.validators).unwrap();
        let mut expected_signatories = SignatorySet::new();
        expected_signatories.set(Signatory {
            pubkey: bitcoin::PublicKey::from_slice(&[
                3, 148, 217, 3, 10, 128, 64, 14, 129, 125, 33, 213, 163, 104, 0, 227, 122, 136, 27,
                45, 207, 44, 64, 24, 35, 166, 166, 118, 25, 12, 200, 183, 98,
            ])
            .unwrap(),
            voting_power: 100,
        });

        let state = State::wrap_store(&mut net.store).unwrap();
        assert_eq!(
            state.current_signatory_set().unwrap(),
            SignatorySetSnapshot {
                time: 123,
                signatories: expected_signatories
            }
        );

        // lots of time has passed, signatory set should be updated
        let mut header: TendermintHeader = Default::default();
        let mut timestamp = Timestamp::new();
        timestamp.set_seconds(1_000_000_000);
        header.set_time(timestamp);
        let action = Action::BeginBlock(header);
        run(&mut net.store, action, &mut net.validators).unwrap();
        let mut expected_signatories = SignatorySet::new();
        let state = State::wrap_store(&mut net.store).unwrap();
        expected_signatories.set(Signatory {
            pubkey: bitcoin::PublicKey::from_slice(&[
                2, 120, 15, 192, 99, 177, 43, 235, 23, 134, 193, 123, 205, 196, 253, 121, 49, 80,
                163, 93, 230, 224, 193, 88, 89, 18, 15, 145, 105, 217, 229, 114, 148,
            ])
            .unwrap(),
            voting_power: 555,
        });
        expected_signatories.set(Signatory {
            pubkey: bitcoin::PublicKey::from_slice(&[
                3, 148, 217, 3, 10, 128, 64, 14, 129, 125, 33, 213, 163, 104, 0, 227, 122, 136, 27,
                45, 207, 44, 64, 24, 35, 166, 166, 118, 25, 12, 200, 183, 98,
            ])
            .unwrap(),
            voting_power: 100,
        });
        assert_eq!(
            state.current_signatory_set().unwrap(),
            SignatorySetSnapshot {
                time: 1_000_000_000,
                signatories: expected_signatories
            }
        );
    }

    #[test]
    #[should_panic(expected = "Merkle root not found for deposit transaction")]
    fn deposit_invalid_height() {
        let mut net = MockNet::new();

        let (tx, proof) = net.create_btc_proof();
        let deposit = DepositTransaction {
            height: 100,
            proof,
            tx,
            block_index: 0,
            recipients: vec![],
        };
        let action = Action::Transaction(Transaction::Deposit(deposit));

        run(&mut net.store, action, &mut net.validators).unwrap();
    }

    #[test]
    #[should_panic(expected = "Proof merkle root does not match chain")]
    fn deposit_invalid_proof() {
        let mut net = MockNet::new();

        let (tx, proof) = net.create_btc_proof();
        let proof = invalidate_proof(proof);

        let deposit = DepositTransaction {
            height: 0,
            proof,
            tx,
            block_index: 0,
            recipients: vec![],
        };
        let action = Action::Transaction(Transaction::Deposit(deposit));

        run(&mut net.store, action, &mut net.validators).unwrap();
    }

    #[test]
    #[should_panic(expected = "Transaction does not contain any deposit outputs")]
    fn deposit_irrelevant() {
        let mut net = MockNet::new();

        let (tx, proof) = net.create_btc_proof();
        let deposit = DepositTransaction {
            height: 0,
            proof,
            tx,
            block_index: 0,
            recipients: vec![vec![123; 33]],
        };
        let action = Action::Transaction(Transaction::Deposit(deposit));

        run(&mut net.store, action, &mut net.validators).unwrap();
    }

    #[test]
    #[should_panic(expected = "Transaction was already processed")]
    fn deposit_duplicate() {
        let tx = build_tx(vec![build_txout(
            100_000_000,
            nomic_signatory_set::output_script(
                &signatories_from_validators(&mock_validator_set()).unwrap(),
                vec![123; 33],
            ),
        )]);
        let block = build_block(vec![tx.clone()]);
        let mut net = MockNet::with_btc_block(block);

        let (_, proof) = net.create_btc_proof();
        let deposit = DepositTransaction {
            height: 0,
            proof,
            tx,
            block_index: 0,
            recipients: vec![vec![123; 33]],
        };
        let action = Action::Transaction(Transaction::Deposit(deposit));

        run(&mut net.store, action.clone(), &mut net.validators).unwrap();
        run(&mut net.store, action, &mut net.validators).unwrap();
    }

    #[test]
    #[should_panic(expected = "Consumed all recipients")]
    fn deposit_no_recipients() {
        let tx = build_tx(vec![build_txout(
            100_000_000,
            nomic_signatory_set::output_script(
                &signatories_from_validators(&mock_validator_set()).unwrap(),
                vec![123; 33],
            ),
        )]);
        let block = build_block(vec![tx.clone()]);
        let mut net = MockNet::with_btc_block(block);

        let (_, proof) = net.create_btc_proof();
        let deposit = DepositTransaction {
            height: 0,
            proof,
            tx,
            block_index: 0,
            recipients: vec![],
        };
        let action = Action::Transaction(Transaction::Deposit(deposit));

        run(&mut net.store, action.clone(), &mut net.validators).unwrap();
    }

    #[test]
    fn deposit_ok() {
        let tx = build_tx(vec![build_txout(
            100_000_000,
            nomic_signatory_set::output_script(
                &signatories_from_validators(&mock_validator_set()).unwrap(),
                vec![123; 33],
            ),
        )]);

        let block = build_block(vec![tx.clone()]);
        let mut net = MockNet::with_btc_block(block);

        let (tx, proof) = net.create_btc_proof();
        let deposit = DepositTransaction {
            height: 0,
            proof,
            tx,
            block_index: 0,
            recipients: vec![vec![123; 33]],
        };
        let action = Action::Transaction(Transaction::Deposit(deposit));

        run(&mut net.store, action.clone(), &mut net.validators).unwrap();
        let state = State::wrap_store(net.store).unwrap();
        // check recipient balance
        assert_eq!(
            state.accounts.get([123; 33]).unwrap().unwrap(),
            Account {
                balance: 100_000_000,
                nonce: 0
            }
        );
    }

    #[test]
    #[should_panic(expected = "Transaction fee is too small")]
    fn transfer_insufficient_fee() {
        let mut net = MockNet::new();

        let (sender_privkey, sender_pubkey) = create_keypair(1);
        let sender_address = sender_pubkey.serialize().to_vec();
        let receiver_address = vec![124; 33];

        let mut state = State::wrap_store(&mut net.store).unwrap();
        state
            .accounts
            .insert(
                unsafe_slice_to_address(sender_address.as_slice()),
                Account {
                    balance: 1234,
                    nonce: 0,
                },
            )
            .unwrap();

        let mut tx = TransferTransaction {
            from: sender_address,
            to: receiver_address.clone(),
            signature: vec![],
            amount: 100,
            nonce: 0,
            fee_amount: 0,
        };
        let sig = sign(&mut tx, sender_privkey);
        tx.signature = sig;

        let action = Action::Transaction(Transaction::Transfer(tx));
        run(&mut net.store, action, &mut net.validators).unwrap();
    }

    #[test]
    #[should_panic(expected = "Account does not exist")]
    fn transfer_from_nonexistent_account() {
        let mut net = MockNet::new();

        let (sender_privkey, sender_pubkey) = create_keypair(1);
        let sender_address = sender_pubkey.serialize().to_vec();
        let receiver_address = vec![124; 33];

        let mut tx = TransferTransaction {
            from: sender_address,
            to: receiver_address.clone(),
            signature: vec![],
            amount: 100,
            nonce: 0,
            fee_amount: 1000,
        };
        let sig = sign(&mut tx, sender_privkey);
        tx.signature = sig;

        let action = Action::Transaction(Transaction::Transfer(tx));
        run(&mut net.store, action, &mut net.validators).unwrap();
    }

    #[test]
    #[should_panic(expected = "Insufficient balance in sender account")]
    fn transfer_insufficient_balance() {
        let mut net = MockNet::new();

        let (sender_privkey, sender_pubkey) = create_keypair(1);
        let sender_address = sender_pubkey.serialize().to_vec();
        let receiver_address = vec![124; 33];

        let mut state = State::wrap_store(&mut net.store).unwrap();
        state
            .accounts
            .insert(
                unsafe_slice_to_address(sender_address.as_slice()),
                Account {
                    balance: 1234,
                    nonce: 0,
                },
            )
            .unwrap();

        let mut tx = TransferTransaction {
            from: sender_address,
            to: receiver_address.clone(),
            signature: vec![],
            amount: 300,
            nonce: 0,
            fee_amount: 1000,
        };
        let sig = sign(&mut tx, sender_privkey);
        tx.signature = sig;

        let action = Action::Transaction(Transaction::Transfer(tx));
        run(&mut net.store, action, &mut net.validators).unwrap();
    }

    #[test]
    #[should_panic(expected = "Invalid account nonce for transaction")]
    fn transfer_invalid_nonce() {
        let mut net = MockNet::new();

        let (sender_privkey, sender_pubkey) = create_keypair(1);
        let sender_address = sender_pubkey.serialize().to_vec();
        let receiver_address = vec![124; 33];

        let mut state = State::wrap_store(&mut net.store).unwrap();
        state
            .accounts
            .insert(
                unsafe_slice_to_address(sender_address.as_slice()),
                Account {
                    balance: 1234,
                    nonce: 100,
                },
            )
            .unwrap();

        let mut tx = TransferTransaction {
            from: sender_address,
            to: receiver_address.clone(),
            signature: vec![],
            amount: 100,
            nonce: 0,
            fee_amount: 1000,
        };
        let sig = sign(&mut tx, sender_privkey);
        tx.signature = sig;

        let action = Action::Transaction(Transaction::Transfer(tx));
        run(&mut net.store, action, &mut net.validators).unwrap();
    }

    #[test]
    #[should_panic(expected = "Invalid signature")]
    fn transfer_invalid_signature() {
        let mut net = MockNet::new();

        let (sender_privkey, sender_pubkey) = create_keypair(1);
        let sender_address = sender_pubkey.serialize().to_vec();
        let receiver_address = vec![124; 33];

        let mut state = State::wrap_store(&mut net.store).unwrap();
        state
            .accounts
            .insert(
                unsafe_slice_to_address(sender_address.as_slice()),
                Account {
                    balance: 1234,
                    nonce: 0,
                },
            )
            .unwrap();

        let mut tx = TransferTransaction {
            from: sender_address,
            to: receiver_address.clone(),
            signature: vec![],
            amount: 100,
            nonce: 0,
            fee_amount: 1000,
        };
        let sig = sign(&mut tx, sender_privkey);
        tx.signature = sig;
        tx.signature[10] ^= 1;

        let action = Action::Transaction(Transaction::Transfer(tx));
        run(&mut net.store, action, &mut net.validators).unwrap();
    }

    #[test]
    fn transfer_ok() {
        let mut net = MockNet::new();

        let (sender_privkey, sender_pubkey) = create_keypair(1);
        let sender_address = sender_pubkey.serialize().to_vec();
        let receiver_address = vec![124; 33];

        let mut state = State::wrap_store(&mut net.store).unwrap();
        state
            .accounts
            .insert(
                unsafe_slice_to_address(sender_address.as_slice()),
                Account {
                    balance: 1234,
                    nonce: 0,
                },
            )
            .unwrap();

        let mut tx = TransferTransaction {
            from: sender_address.clone(),
            to: receiver_address.clone(),
            signature: vec![],
            amount: 100,
            nonce: 0,
            fee_amount: 1000,
        };
        let sig = sign(&mut tx, sender_privkey);
        tx.signature = sig;

        let action = Action::Transaction(Transaction::Transfer(tx));
        run(&mut net.store, action, &mut net.validators).unwrap();

        let state = State::wrap_store(&mut net.store).unwrap();
        assert_eq!(
            state
                .accounts
                .get(unsafe_slice_to_address(&receiver_address[..]))
                .unwrap()
                .unwrap(),
            Account {
                balance: 100,
                nonce: 0
            }
        );
        assert_eq!(
            state
                .accounts
                .get(unsafe_slice_to_address(&sender_address[..]))
                .unwrap()
                .unwrap(),
            Account {
                balance: 134,
                nonce: 1
            }
        );
    }
    // TODO: test for transfer to self

    #[test]
    fn withdrawal_ok() {
        let mut net = MockNet::new();

        let (sender_privkey, sender_pubkey) = create_keypair(1);
        let sender_address = sender_pubkey.serialize().to_vec();

        let mut state = State::wrap_store(&mut net.store).unwrap();
        state
            .accounts
            .insert(
                unsafe_slice_to_address(sender_address.as_slice()),
                Account {
                    balance: 1234,
                    nonce: 0,
                },
            )
            .unwrap();

        let mut tx = WithdrawalTransaction {
            from: sender_address.clone(),
            to: bitcoin::Script::from(vec![123]),
            amount: 1000,
            signature: vec![],
            nonce: 0,
        };
        let sig = sign(&mut tx, sender_privkey);
        tx.signature = sig;
        let action = Action::Transaction(Transaction::Withdrawal(tx));
        run(&mut net.store, action, &mut net.validators).unwrap();

        let state = State::wrap_store(&mut net.store).unwrap();
        assert_eq!(
            state
                .accounts
                .get(unsafe_slice_to_address(&sender_address[..]))
                .unwrap()
                .unwrap(),
            Account {
                balance: 234,
                nonce: 1,
            }
        );
        assert_eq!(state.pending_withdrawals.get(0).unwrap().value, 1000);
    }

    #[test]
    #[should_panic(expected = "Invalid signature")]
    fn withdrawal_invalid_signature() {
        let mut net = MockNet::new();

        let (sender_privkey, sender_pubkey) = create_keypair(1);
        let sender_address = sender_pubkey.serialize().to_vec();

        let mut state = State::wrap_store(&mut net.store).unwrap();
        state
            .accounts
            .insert(
                unsafe_slice_to_address(sender_address.as_slice()),
                Account {
                    balance: 1234,
                    nonce: 0,
                },
            )
            .unwrap();

        let mut tx = WithdrawalTransaction {
            from: sender_address.clone(),
            to: bitcoin::Script::from(vec![123]),
            amount: 1000,
            signature: vec![],
            nonce: 0,
        };
        let sig = sign(&mut tx, sender_privkey);
        tx.signature = sig;
        tx.signature[10] ^= 1;
        let action = Action::Transaction(Transaction::Withdrawal(tx));
        run(&mut net.store, action, &mut net.validators).unwrap();
    }

    #[test]
    #[should_panic(expected = "Invalid account nonce for withdrawal transaction")]
    fn withdrawal_invalid_nonce() {
        let mut net = MockNet::new();

        let (sender_privkey, sender_pubkey) = create_keypair(1);
        let sender_address = sender_pubkey.serialize().to_vec();

        let mut state = State::wrap_store(&mut net.store).unwrap();
        state
            .accounts
            .insert(
                unsafe_slice_to_address(sender_address.as_slice()),
                Account {
                    balance: 1234,
                    nonce: 100,
                },
            )
            .unwrap();

        let mut tx = WithdrawalTransaction {
            from: sender_address.clone(),
            to: bitcoin::Script::from(vec![123]),
            amount: 1000,
            signature: vec![],
            nonce: 0,
        };
        let sig = sign(&mut tx, sender_privkey);
        tx.signature = sig;
        let action = Action::Transaction(Transaction::Withdrawal(tx));
        run(&mut net.store, action, &mut net.validators).unwrap();
    }

    #[test]
    #[should_panic(expected = "Insufficient balance in sender account")]
    fn withdrawal_insufficient_balance() {
        let mut net = MockNet::new();

        let (sender_privkey, sender_pubkey) = create_keypair(1);
        let sender_address = sender_pubkey.serialize().to_vec();

        let mut state = State::wrap_store(&mut net.store).unwrap();
        state
            .accounts
            .insert(
                unsafe_slice_to_address(sender_address.as_slice()),
                Account {
                    balance: 1234,
                    nonce: 0,
                },
            )
            .unwrap();

        let mut tx = WithdrawalTransaction {
            from: sender_address.clone(),
            to: bitcoin::Script::from(vec![123]),
            amount: 2000,
            signature: vec![],
            nonce: 0,
        };
        let sig = sign(&mut tx, sender_privkey);
        tx.signature = sig;
        let action = Action::Transaction(Transaction::Withdrawal(tx));
        run(&mut net.store, action, &mut net.validators).unwrap();
    }

    #[test]
    #[should_panic(expected = "Account does not exist")]
    fn withdrawal_from_nonexistent_account() {
        let mut net = MockNet::new();

        let (sender_privkey, sender_pubkey) = create_keypair(1);
        let sender_address = sender_pubkey.serialize().to_vec();

        let mut tx = WithdrawalTransaction {
            from: sender_address.clone(),
            to: bitcoin::Script::from(vec![123]),
            amount: 1000,
            signature: vec![],
            nonce: 0,
        };
        let sig = sign(&mut tx, sender_privkey);
        tx.signature = sig;
        let action = Action::Transaction(Transaction::Withdrawal(tx));
        run(&mut net.store, action, &mut net.validators).unwrap();
    }
}
