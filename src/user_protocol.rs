use std::collections::HashSet;
use std::str::FromStr;
use bdk::bitcoin::hashes::{Hash, sha256};
use bdk::bitcoin::psbt::Psbt;
use bdk::bitcoin::{Address, Network, OutPoint, PrivateKey, PublicKey, Sequence, Txid};
use bdk::bitcoin::secp256k1::Secp256k1;
use bdk::descriptor::Descriptor;
use bdk::wallet::{AddressIndex, get_funded_wallet};
use bdk::{KeychainKind, LocalUtxo, SignOptions, Wallet};
use bdk::database::{AnyDatabase, MemoryDatabase};
use bdk::psbt::PsbtUtils;
use joinswap::{check_prv_keys, users2maker_contract_desc, gen_key_pair, get_descriptors, read_contract_keys, read_message, read_psbt, maker2users_contract_desc, send_message, sign_and_send_psbt};

use serde_json;
use tokio::io::{BufReader, ReadHalf, split, WriteHalf};
use tokio::net::TcpStream;

#[tokio::main]
async fn main() {
    let socket = TcpStream::connect("127.0.0.1:8080").await.unwrap();
    let (reader, writer) = split(socket);
    let reader = BufReader::new(reader);
    println!("CONNECT TO MAKER 👉👈\n");

    // Later, a new pair of writer/reader will be pushed into these vectors to communicate with the
    // maker using different identities (second part of a regular CoinJoin)
    let mut writer = vec![writer];
    let mut reader = vec![reader];

    let (prv_key1, pub_key1) = gen_key_pair();
    let (prv_key2, pub_key2) = gen_key_pair();
    let (prv_key3, pub_key3) = gen_key_pair();

    let (user_wallet, _, _) = get_funded_wallet(&get_descriptors());
    let (my_utxo, refund) = send_user_data(
        &user_wallet, &pub_key1, &pub_key2, &pub_key3,
        &mut writer[0]).await;

    println!("User data ----------------------------> Maker\n");
    println!("CONTRACT CREATION 🐸\n");

    let (keys, hash) = read_contract_data(&mut reader[0]).await;
    let mut funding_psbt = read_psbt(&mut reader[0], None).await;
    let mut refund_psbt = read_psbt(&mut reader[0], None).await;

    println!("Contract data <------------------------ Maker");
    println!("Funding and Refund Tx <---------------- Maker\n");

    // There should be no duplicate keys and my keys should appear once in each policy path
    check_contract_keys(&keys, &pub_key1, &pub_key2, &pub_key3);

    let users2maker_desc_str = users2maker_contract_desc(&keys, hash);
    let users2maker_desc = Descriptor::<PublicKey>::from_str(&users2maker_desc_str).unwrap();
    println!("Users-to-maker contract address:\n{}\n",
             users2maker_desc.address(Network::Regtest).unwrap());

    // Ensure the funding and refund psbts are correctly formed
    check_psbts(&funding_psbt, &refund_psbt, &users2maker_desc, my_utxo, &refund);

    // The refund tx spends from the contract, so to sign it we use our contract private keys
    let users2maker_prv_desc = users2maker_desc_str
        .replace(&pub_key1.to_string(), &prv_key1.to_string())
        .replace(&pub_key2.to_string(), &prv_key2.to_string())
        .replace(&pub_key3.to_string(), &prv_key3.to_string());

    let prv_wallet = Wallet::new(
        &users2maker_prv_desc,
        None,
        Network::Regtest,
        MemoryDatabase::new(),
    ).unwrap();

    let sign_ops = SignOptions { trust_witness_utxo: true, ..Default::default() };
    sign_and_send_psbt(&mut refund_psbt, &prv_wallet, sign_ops, &mut writer).await;
    println!("Signed Refund PSBTs ------------------> Maker");

    let _refund_final = read_psbt(&mut reader[0], Some(refund_psbt.unsigned_tx.txid())).await;
    // Here we should verify the refund tx is valid and can be mined
    println!("Finalized Refund Tx <------------------ Maker\n");

    // Now that we have the finalized refund tx that is valid after a relative timelock we can sign
    // the funding tx without risk of losing the funds
    sign_and_send_psbt(&mut funding_psbt, &user_wallet, SignOptions::default(), &mut writer).await;
    println!("Signed Funding PSBTs -----------------> Maker");

    let _funding_final = read_psbt(&mut reader[0], Some(funding_psbt.unsigned_tx.txid())).await;
    println!("Finalized Funding Tx <----------------- Maker\n");

    // Here we should wait the funding tx to be mined, or broadcast it ourselves
    println!("Broadcast Funding Tx\n");

    // Connect to the maker with a different ID for the second leg of the JoinSwap
    let socket = TcpStream::connect("127.0.0.1:8080").await.unwrap();
    let (reader_new, writer_new) = split(socket);
    let reader_new = BufReader::new(reader_new);
    println!("CONNECT TO MAKER (NEW ID) 👉👈\n");

    writer.push(writer_new);
    reader.push(reader_new);

    let (prv_key4, pub_key4) = gen_key_pair();
    let (_prv_key5, pub_key5) = gen_key_pair();

    // Note that we use writer[1] to write to the maker with the new ID
    send_second_user_data(&pub_key4, &pub_key5, &mut writer[1]).await;
    println!("User data ------------NEW-ID----------> Maker\n");

    println!("SECOND CONTRACT CREATION 🐸\n");
    // Read maker pub keys and txid and derive the maker2user contract descriptor
    let ((maker_key1, maker_key2), _txid) = read_second_contract_data(&mut reader[1]).await;
    println!("Maker2user contract + TxID <---NEW-ID-- Maker\n");

    let maker2user_desc_str = maker2users_contract_desc(
        &[pub_key4, maker_key1],
        &maker_key2,
        &pub_key5,
        hash,
    );
    let maker2user_desc = Descriptor::<PublicKey>::from_str(&maker2user_desc_str).unwrap();
    println!("Maker-to-user contract address:\n{}\n",
             maker2user_desc.address(Network::Regtest).unwrap());

    // Fetch the maker2user tx from the blockchain using the txid and check it has an output that
    // matches the descriptor spk with the correct balance
    println!("Fetch maker-to-user transaction\n");

    // If the previous step was successful, send the hashlock path private key from the users2maker
    // contract to the maker. If all users agree that maker funded correctly the maker2users
    // contracts then maker will have all the hashlock path keys, and so will be able to spend the
    // first contract coins by revealing the preimage.

    // This private key must be sent with the old ID (such that the two IDs remain unlinked)
    send_prv_key(&prv_key3, &mut writer[0]).await;
    println!("PRIVATE KEYS HANDOVER 😎🤝😎\n");
    println!("Users2maker hashlock path PrvKey -----> Maker");

    // Read preimage + maker2user contract prv key and check them
    // If correct, users can now redeem the maker2user contract coins
    let (preimage, maker_prv_key) = read_preimage_and_prv_key(&mut reader[1]).await;
    println!("Maker2user contract PrvKey <---NEW-ID-- Maker");

    assert_eq!(sha256::Hash::hash(&preimage), hash);
    check_prv_keys(&vec![maker_prv_key], vec![maker_key1]);

    // User can now spend from:
    let _maker2user_prv_desc = maker2user_desc_str
        .replace(&pub_key4.to_string(), &prv_key4.to_string())
        .replace(&maker_key1.to_string(), &maker_prv_key.to_string());

    // Send users2maker contract key (with old ID)
    send_prv_key(&prv_key1, &mut writer[0]).await;
    println!("Users2maker contract PrvKey ----------> Maker");

    println!("\nSuccesful JoinSwap! 🙈");
}

async fn read_preimage_and_prv_key(
    reader: &mut BufReader<ReadHalf<TcpStream>>
) -> ([u8; 32], PrivateKey) {
    let preimage_str = read_message(reader).await;
    let preimage: [u8; 32] = serde_json::from_str(preimage_str.trim()).unwrap();

    let prv_key_str = read_message(reader).await;
    let prv_key = PrivateKey::from_str(prv_key_str.trim()).unwrap();

    (preimage, prv_key)
}

async fn send_prv_key(key: &PrivateKey, writer: &mut WriteHalf<TcpStream>) {
    send_message(format!("{}", key), writer).await;
}

async fn read_second_contract_data(
    reader: &mut BufReader<ReadHalf<TcpStream>>
) -> ((PublicKey, PublicKey), Txid) {
    let maker_keys = read_contract_keys(reader, 2).await;

    let txid_str = read_message(reader).await;
    let txid = Txid::from_str(txid_str.trim()).unwrap();
    assert_ne!(maker_keys[0], maker_keys[1]);

    ((maker_keys[0], maker_keys[1]), txid)
}

// This fn should also take the contract value in the future
async fn send_second_user_data(
    key1: &PublicKey,
    key2: &PublicKey,
    writer: &mut WriteHalf<TcpStream>,
) {
    send_message(format!("{},{}", key1, key2), writer).await;
}

async fn send_user_data(
    wallet: &Wallet<AnyDatabase>,
    key1: &PublicKey,
    key2: &PublicKey,
    key3: &PublicKey,
    writer: &mut WriteHalf<TcpStream>,
) -> (LocalUtxo, Address) {
    send_message(format!("{},{},{}", key1, key2, key3), writer).await;
    // We only use the first utxo from the wallet and spent fully for now
    let my_utxo = send_utxo_data(&wallet, writer).await;
    let refund = wallet.get_address(AddressIndex::New).unwrap().address;
    send_message(refund.to_string(), writer).await;

    (my_utxo, refund)
}

async fn read_contract_data(
    reader: &mut BufReader<ReadHalf<TcpStream>>
) -> ([PublicKey; 9], sha256::Hash) {
    let keys = read_contract_keys(reader, 9).await;
    let keys_array = [keys[0], keys[1], keys[2], keys[3], keys[4], keys[5], keys[6], keys[7], keys[8]];

    let hash_str = read_message(reader).await;
    let hash = sha256::Hash::from_str(&hash_str.trim()).unwrap();

    (keys_array, hash)
}

async fn send_utxo_data(wallet: &Wallet<AnyDatabase>, writer: &mut WriteHalf<TcpStream>) -> LocalUtxo {
    let utxos = wallet.list_unspent().unwrap();

    // We fully spend one utxo for now
    let outpoint = utxos[0].outpoint;

    let psbt_in = wallet
        .get_psbt_input(utxos[0].clone(), None, false)
        .unwrap();
    let psbt_in_serialized = serde_json::to_string(&psbt_in).unwrap();

    // Find the concrete descriptor of our utxo
    let pub_desc = wallet.public_descriptor(KeychainKind::External).unwrap().unwrap();
    let (_, desc) = pub_desc.find_derivation_index_for_spk(
        &Secp256k1::new(),
        &utxos[0].txout.script_pubkey,
        0..1
    ).unwrap().unwrap();

    send_message(desc.to_string(), writer).await;
    send_message(outpoint.to_string(), writer).await;
    send_message(psbt_in_serialized, writer).await;

    utxos[0].clone()
}

// Check that all keys are different and that my respective key appears only once per policy path
fn check_contract_keys(
    keys: &[PublicKey; 9],
    my_key1: &PublicKey,
    my_key2: &PublicKey,
    my_key3: &PublicKey,
) {
    assert_eq!(keys.len(), keys.iter().collect::<HashSet<_>>().len());

    assert_eq!(keys[0..3].iter().filter(|&key| key == my_key1).count(), 1);
    assert_eq!(keys[3..6].iter().filter(|&key| key == my_key2).count(), 1);
    assert_eq!(keys[6..9].iter().filter(|&key| key == my_key3).count(), 1);
}

// Check that funding and refund transactions are properly constructed
// (As of now funding tx must have only one output):

// 1. The spk of the funding utxo must match the contract descriptor's
// 2. Fee must be lower than 420 (to be changed in the future with RBF or something)
// 3. My utxo must be included in the inputs once
// 4. Total input value minus funding tx fee must match the output value
// 5. Refund tx input must only be the funding utxo
// 6. Refund tx must spend from the relative timelocked path (actually I don't know how to do that,
// but we can enforce the relative timelock anyway)
// 7. Refund tx must include my address once
// 8. Finally my address must receive initial_amount - (funding_fee + refund_fee)/users
fn check_psbts(
    funding: &Psbt,
    refund: &Psbt,
    desc: &Descriptor<PublicKey>,
    my_utxo: LocalUtxo,
    refund_addr: &Address,
) {
    // 1)
    assert_eq!(funding.unsigned_tx.output[0].script_pubkey, desc.script_pubkey());

    // 2)
    let funding_fee = funding.fee_amount().unwrap();
    assert!(funding_fee < 420);

    // for each input of the funding tx, get the prev output (OutPoint)
    let prevouts = funding.unsigned_tx.input
        .iter()
        .map(|txin| txin.previous_output);

    // 3)
    let my_utxo_outpoint: Vec<_> = prevouts.clone()
        .filter(|prevout| *prevout == my_utxo.outpoint)
        .collect();
    assert_eq!(my_utxo_outpoint.len(), 1);

    // for each input, index the output of the specific tx to get the utxo value
    let input_values = funding.inputs
        .iter()
        .zip(prevouts)
        .map(|(input, prevout)| {
            let vout = prevout.vout as usize;
            input.non_witness_utxo.as_ref().unwrap().output[vout].value.clone()
        });

    // 4)
    let total_input_value: u64 = input_values.sum();
    assert_eq!(total_input_value - funding_fee, funding.unsigned_tx.output[0].value);

    // 5)
    let funding_outpoint = OutPoint { txid: funding.unsigned_tx.txid(), vout: 0 };
    assert_eq!(refund.inputs.len(), 1);
    assert_eq!(refund.unsigned_tx.input[0].previous_output, funding_outpoint);

    // 6)
    assert_eq!(refund.unsigned_tx.version, 2);
    assert_eq!(refund.unsigned_tx.input[0].sequence, Sequence::from_height(48));

    // 7)
    let my_txout: Vec<_> = refund.unsigned_tx.output.iter().filter(|txout| {
        txout.script_pubkey == refund_addr.script_pubkey()
    }).collect();
    assert_eq!(my_txout.len(), 1);

    // 8)
    let users = refund.outputs.iter().count() as u64;
    assert_eq!(refund.fee_amount().unwrap(), 1000);
    let refund_amount = my_utxo.txout.value - (&funding_fee + 1000)/users;
    assert_eq!(my_txout[0].value, refund_amount);
}