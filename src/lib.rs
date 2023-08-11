use std::collections::BTreeMap;
use std::str::FromStr;

use bdk::bitcoin::{Address, Network, OutPoint, PrivateKey, PublicKey, Txid};
use bdk::bitcoin::psbt::Psbt;
use bdk::descriptor::{Descriptor, Segwitv0};
use bdk::{KeychainKind, LocalUtxo, SignOptions, Utxo, Wallet, WeightedUtxo};
use bdk::bitcoin::hashes::sha256;
use bdk::bitcoin::secp256k1::Secp256k1;
use bdk::bitcoin::util::bip32::{DerivationPath, KeySource};
use bdk::database::{BatchDatabase, BatchOperations, MemoryDatabase};

use bdk::keys::{GeneratedKey, GeneratableKey, ExtendedKey, DerivableKey, DescriptorKey, PrivateKeyGenerateOptions};
use bdk::keys::bip39::{Language, Mnemonic, WordCount};
use bdk::keys::DescriptorKey::Secret;
use bdk::psbt::PsbtUtils;
use bdk::wallet::AddressIndex;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, ReadHalf, WriteHalf};
use tokio::net::TcpStream;

pub fn check_prv_keys(prv_keys: &Vec<PrivateKey>, match_against: Vec<PublicKey>) {
    let secp = Secp256k1::new();

    let pub_keys = prv_keys.iter()
        .map(|key| key.public_key(&secp));

    pub_keys.for_each(|key| {
        assert_eq!(match_against.iter().filter(|actual_key| **actual_key == key).count(), 1)
    });
}

// The first pair of keys is from the user and maker, timelocked path key is from maker, and
// hashlocked path key is from user
pub fn maker2users_contract_desc(
    multisig_keys: &[PublicKey; 2],
    timelock_key: &PublicKey,
    hashlock_key: &PublicKey,
    hash: sha256::Hash,
) -> String {
format!("wsh(thresh(1,\
    multi(2,{},{}),\
    snj:and_v(v:pk({}),older(69)),\
    aj:and_v(v:pk({}),sha256({hash}))\
    ))", multisig_keys[0], multisig_keys[1], timelock_key, hashlock_key)
}

// Each triplet of keys must be from the users A, B and the maker
pub fn users2maker_contract_desc(keys: &[PublicKey; 9], hash: sha256::Hash) -> String {
    format!("wsh(thresh(1,\
    multi(3,{},{},{}),\
    anj:and_v(v:multi(3,{},{},{}),older(48)),\
    aj:and_v(v:multi(3,{},{},{}),sha256({hash}))\
    ))", keys[0], keys[1], keys[2], keys[3], keys[4], keys[5], keys[6], keys[7], keys[8])
}

pub async fn read_contract_keys(reader: &mut BufReader<ReadHalf<TcpStream>>, n: u8) -> Vec<PublicKey> {
    let line = read_message(reader).await;
    let parts: Vec<&str> = line.trim().split(',').collect();

    if parts.len() != n as usize {
        panic!("Invalid input! Please ensure there are {n} pub keys separated only by commas");
    }

    parts.iter().map(|key| {
        PublicKey::from_str(key).unwrap()
    }).collect()
}

pub async fn send_message(m: String, writer: &mut WriteHalf<TcpStream>) {
    let line = m+"\n";
    writer.write_all(line.as_bytes()).await.unwrap();
}

pub async fn read_message(reader: &mut BufReader<ReadHalf<TcpStream>>) -> String {
    let mut buf = String::new();
    reader.read_line(&mut buf).await.unwrap();

    buf
}

pub async fn read_psbt(
    reader: &mut BufReader<ReadHalf<TcpStream>>,
    txid: Option<Txid>,
) -> Psbt {
    let line = read_message(reader).await;
    let psbt: Psbt = serde_json::from_str(&line.trim()).unwrap();

    if let Some(value) = txid {
        assert_eq!(psbt.unsigned_tx.txid(), value);
    }
    psbt
}

pub async fn sign_and_send_psbt<D: BatchDatabase>(
    psbt: &mut Psbt,
    wallet: &Wallet<D>,
    sign_ops: SignOptions,
    writers: &mut Vec<WriteHalf<TcpStream>>,
) {
    wallet.sign(psbt, sign_ops).unwrap();
    let serialized_psbt = serde_json::to_string(psbt).unwrap();

    for mut writer in writers {
        send_message(serialized_psbt.to_string(), &mut writer).await;
    }
}

pub fn build_funding_and_refund(
    pub_desc: &Descriptor<PublicKey>,
    from_utxos: Vec<WeightedUtxo>,
    refund_to: Vec<Address>,
) -> (Psbt, Psbt) {
    assert_eq!(from_utxos.len(), refund_to.len());
    assert!(pub_desc.sanity_check().is_ok());

    let initial_amounts = (0..from_utxos.len())
        .into_iter()
        .map(|i| from_utxos[i].utxo.txout().value);

    let refund_recipients: Vec<(Address, u64)> = refund_to
        .into_iter()
        .zip(initial_amounts)
        .collect();

    let pub_wallet = Wallet::new(
        &pub_desc.to_string(),
        None,
        Network::Regtest,
        MemoryDatabase::new(),
    ).unwrap();
    let funding_psbt = build_funding_tx(&pub_wallet, from_utxos);

    // Create local utxo with the funding tx and update the database (only one output assumed)
    let outpoint = OutPoint { txid: funding_psbt.unsigned_tx.txid(), vout: 0 };
    let local = LocalUtxo {
        outpoint,
        txout: funding_psbt.unsigned_tx.output[0].clone(),
        keychain: KeychainKind::External,
        is_spent: false
    };
    let mut database = MemoryDatabase::new();
    database.set_utxo(&local).unwrap();

    let updated_wallet = Wallet::new(
        &pub_desc.to_string(),
        None,
        Network::Regtest,
        database,
    ).unwrap();

    let mut refund_psbt = build_refund_tx(&updated_wallet, refund_recipients, &funding_psbt);

    // Witness utxo field doesn't include the whole tx data so we can spend from unsigned txs
    refund_psbt.inputs[0].witness_utxo = Some(funding_psbt.unsigned_tx.output[0].clone());

    (funding_psbt, refund_psbt)
}

fn build_refund_tx(
    wallet: &Wallet<MemoryDatabase>,
    recipients: Vec<(Address, u64)>,
    funding_psbt: &Psbt,
) -> Psbt {
    assert_eq!(recipients.len(), funding_psbt.unsigned_tx.input.len());
    let out_count = recipients.len() as u64;

    let funding_fee = funding_psbt.fee_amount().unwrap();
    let refund_fee = 1000;

    let mut outputs = Vec::new();
    for (address, initial_value) in recipients {
        let final_value =
            initial_value - (&funding_fee / &out_count) - (&refund_fee / &out_count);

        outputs.push((address.script_pubkey(), final_value));
    }

    // We have to spend from the relative timelocked path
    let mut path = BTreeMap::new();
    let wallet_policy = wallet.policies(KeychainKind::External).unwrap().unwrap();
    path.insert(wallet_policy.id, vec![1]);

    let outpoint = OutPoint { txid: funding_psbt.unsigned_tx.txid(), vout: 0 };
    let mut tx_builder = wallet.build_tx();
    tx_builder
        .manually_selected_only()
        .add_utxo(outpoint).unwrap()
        .fee_absolute(refund_fee)
        .set_recipients(outputs)
        .policy_path(path, KeychainKind::External);

    let (psbt, _) = tx_builder.finish().unwrap();

    psbt
}

fn build_funding_tx(
    receive_wallet: &Wallet<MemoryDatabase>,
    utxos: Vec<WeightedUtxo>,
) -> Psbt {
    let mut tx_builder = receive_wallet.build_tx();
    tx_builder.manually_selected_only();

    for utxo in utxos {
        match utxo.utxo {
            Utxo::Foreign { outpoint, psbt_input } => {
                tx_builder.add_foreign_utxo(outpoint, *psbt_input, utxo.satisfaction_weight).unwrap();
            },
            Utxo::Local(_) => {
                panic!("FUUUCK EL UTXO ES LOCAL");
            },
        }
    }
    let wallet_address = receive_wallet.get_address(AddressIndex::New).unwrap();
    tx_builder.drain_to(wallet_address.script_pubkey());

    // To build a tx from the wallet we need to specify the policy path although we are not
    // spending from our own wallet UTXOs
    let mut path = BTreeMap::new();
    let wallet_policy = receive_wallet.policies(KeychainKind::External).unwrap().unwrap();
    path.insert(wallet_policy.id, vec![0]);
    tx_builder.policy_path(path, KeychainKind::External);

    let (psbt, _) = tx_builder.finish().unwrap();

    psbt
}

pub fn gen_key_pair() -> (PrivateKey, PublicKey) {
    let secp = Secp256k1::new();

    let key: GeneratedKey<_, Segwitv0> =
        PrivateKey::generate(PrivateKeyGenerateOptions::default()).unwrap();

    let pubk = key.public_key(&secp);
    let privk = key.into_key();

    (privk, pubk)
}

pub fn get_descriptors() -> String {
    let secp = Secp256k1::new();

    let password = Some("watafak".to_string());

    let mnemonic: GeneratedKey<_, Segwitv0> =
        Mnemonic::generate((WordCount::Words12, Language::English)).unwrap();
    let mnemonic = mnemonic.into_key();

    let xkey: ExtendedKey = (mnemonic, password).into_extended_key().unwrap();
    let xprv = xkey.into_xprv(Network::Regtest).unwrap();

    let mut keys = Vec::new();

    for path in ["m/84h/1h/0h/0", "m/84h/1h/0h/1"] {
        let deriv_path = DerivationPath::from_str(path).unwrap();
        let derived_xprv = &xprv.derive_priv(&secp, &deriv_path).unwrap();
        let origin: KeySource = (xprv.fingerprint(&secp), deriv_path);
        let derived_xprv_desc_key: DescriptorKey<Segwitv0> =
            derived_xprv.into_descriptor_key(Some(origin), DerivationPath::default()).unwrap();

        // Wrap the derived key with the wpkh() string to produce a descriptor string
        if let Secret(key, _, _) = derived_xprv_desc_key {
            let mut desc = "wpkh(".to_string();
            desc.push_str(&key.to_string());
            desc.push_str(")");
            keys.push(desc);
        }
    }

    keys[0].clone()
}