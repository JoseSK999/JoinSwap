use std::str::FromStr;
use bdk::bitcoin::{Address, Network, OutPoint, PrivateKey, psbt, PublicKey, Txid};
use bdk::descriptor::Descriptor;
use bdk::{SignOptions, Utxo, Wallet, WeightedUtxo};
use bdk::bitcoin::hashes::{Hash, sha256};
use bdk::bitcoin::psbt::Psbt;
use bdk::bitcoin::secp256k1::rand::{thread_rng, Rng};
use bdk::database::{AnyDatabase, MemoryDatabase};
use bdk::psbt::PsbtUtils;
use bdk::wallet::get_funded_wallet;

use serde_json;
use tokio::io::{BufReader, ReadHalf, split, WriteHalf};
use tokio::net::{TcpListener, TcpStream};

use joinswap::{build_funding_and_refund, check_prv_keys, users2maker_contract_desc, gen_key_pair, get_descriptors, read_contract_keys, read_message, read_psbt, maker2users_contract_desc, send_message, sign_and_send_psbt};

#[tokio::main]
async fn main() {
    let listener = TcpListener::bind("127.0.0.1:8080").await.unwrap();

    // Accept the connections from user A and B
    println!("CONNECTIONS 👉👈\n");
    let (mut reader_a, writer_a) = accept_connection(&listener).await;
    println!("New connection <-----------------> User A");
    let (mut reader_b, writer_b) = accept_connection(&listener).await;
    println!("New connection <-----------------> User B");

    let ((key1_a, key2_a, key3_a), weighted_a, addr_a) = read_user_data(&mut reader_a).await;
    let ((key1_b, key2_b, key3_b), weighted_b, addr_b) = read_user_data(&mut reader_b).await;
    println!("User data <----------------------- Users (A/B)\n");

    let mut writers = vec![writer_a, writer_b];
    let mut readers = vec![reader_a, reader_b];

    // Maker keys used in the contract
    let (prv_key1, pub_key1) = gen_key_pair();
    let (prv_key2, pub_key2) = gen_key_pair();
    let (prv_key3, pub_key3) = gen_key_pair();

    // Each 3 keys are from a different multisig path in the contract
    let keys = [key1_a, key1_b, pub_key1, key2_a, key2_b, pub_key2, key3_a, key3_b, pub_key3];
    let (preimage, hash) = gen_hash();

    let users2maker_desc_str = users2maker_contract_desc(&keys, hash);
    let users2maker_desc = Descriptor::<PublicKey>::from_str(&users2maker_desc_str).unwrap();

    println!("CONTRACT CREATION 🐸\n");
    println!("Users-to-maker contract address:\n{}\n",
             users2maker_desc.address(Network::Regtest).unwrap());

    // Build funding and refund tx spending from user utxos and refunding to their addresses
    let (funding_psbt, refund_psbt) = build_funding_and_refund(
        &users2maker_desc,
        vec![weighted_a, weighted_b],
        vec![addr_a, addr_b],
    );

    send_contract_data(&keys, hash, &funding_psbt, &refund_psbt, &mut writers).await;
    println!("Contract data -------------------> Users (A/B)");
    println!("Funding and Refund Tx -----------> Users (A/B)\n");

    // Combine the signed refund psbts received from the users
    let mut refund_final = read_and_combine_psbt(
        &mut readers, Some(refund_psbt.unsigned_tx.txid())).await;
    println!("Signed Refund PSBTs <------------- Users (A/B)");

    // We have to sign from the refund psbt too as our key is also in the contract
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
    sign_and_send_psbt(&mut refund_final, &prv_wallet, sign_ops, &mut writers).await;
    println!("Finalized Refund Tx -------------> Users (A/B)\n");

    // Now that users have the finalized refund tx they sign the funding tx
    let funding_final = read_and_combine_psbt(&mut readers, Some(funding_psbt.unsigned_tx.txid())).await;
    println!("Signed Funding PSBTs <------------ Users (A/B)");
    send_psbt(&funding_final, &mut writers).await;
    println!("Finalized Funding Tx ------------> Users (A/B)\n");

    // Here we should broadcast the funding tx and wait
    println!("Broadcast Funding Tx\n");

    // Second leg of the JoinSwap: The new peers should give us a blinded certificate to ensure
    // they are the same participants
    println!("CONNECTIONS, SECOND PART 👉👈\n");
    let (mut reader_x, writer_x) = accept_connection(&listener).await;
    println!("New connection <-----------------> User X");
    let (mut reader_y, writer_y) = accept_connection(&listener).await;
    println!("New connection <-----------------> User Y");

    let (key1_x, key2_x) = read_second_user_data(&mut reader_x).await;
    let (key1_y, key2_y) = read_second_user_data(&mut reader_y).await;
    println!("User data <----------------------- Users (X/Y)\n");

    // We will use the old IDs to read the users2maker contract private keys (private key handover)
    let mut old_readers = readers;
    let mut new_writers = vec![writer_x, writer_y];

    // Gen maker keys and build the descriptor for each maker2user contract
    let (prv_key4, pub_key4) = gen_key_pair();
    let (_prv_key5, pub_key5) = gen_key_pair();
    let (prv_key6, pub_key6) = gen_key_pair();
    let (_prv_key7, pub_key7) = gen_key_pair();

    let maker2user_x_desc_str = maker2users_contract_desc(
        &[key1_x, pub_key4],
        &pub_key5,
        &key2_x,
        hash);
    let maker2user_y_desc_str = maker2users_contract_desc(
        &[key1_y, pub_key6],
        &pub_key7,
        &key2_y,
        hash);
    let maker2user_x_desc = Descriptor::<PublicKey>::from_str(&maker2user_x_desc_str).unwrap();
    let maker2user_y_desc = Descriptor::<PublicKey>::from_str(&maker2user_y_desc_str).unwrap();

    println!("SECOND CONTRACT CREATION 🐸\n");
    println!("Maker-to-user X contract address:\n{}\n",
             maker2user_x_desc.address(Network::Regtest).unwrap());
    println!("Maker-to-user Y contract address:\n{}\n",
             maker2user_y_desc.address(Network::Regtest).unwrap());

    // Build and sign the funding tx for each maker2user contract
    let mut total_spent = 0;
    let maker2users_txs: Vec<_> = [maker2user_x_desc, maker2user_y_desc].iter().map(|desc| {
        let (wallet, _, _) = get_funded_wallet(&get_descriptors());
        let mut psbt = build_second_funding(&wallet, &desc);

        psbt.unsigned_tx.output.iter()
            .filter(|txout| txout.script_pubkey == desc.script_pubkey())
            .for_each(|txout| total_spent += txout.value);
        total_spent += psbt.fee_amount().unwrap();

        let finalized = wallet.sign(&mut psbt, SignOptions::default()).unwrap();
        assert!(finalized);

        psbt.extract_tx()
    }).collect();

    // Here these txs should be broadcast and mined within a period of time
    println!("Broadcast maker-to-user X transaction");
    println!("Broadcast maker-to-user Y transaction");

    // Send maker pub keys + tx id to each user
    send_second_contract_data(
        vec![&[pub_key4, pub_key5], &[pub_key6, pub_key7]],
        vec![maker2users_txs[0].txid(), maker2users_txs[1].txid()],
        &mut new_writers,
    ).await;
    println!("Maker2users contract + TxIDs ----> Users (X/Y)\n");

    // Once that users verify the funding second contract txs, they send us their private keys from
    // the hashlock path of the users2maker contract. We then can redeem the first contract coins by
    // revealing the preimage.

    let hashlock_prv_keys = read_prv_keys(&mut old_readers).await;
    println!("PRIVATE KEYS HANDOVER 😎🤝😎\n");
    println!("Users2maker hashlock PrvKeys <---- Users (A/B)");

    // Check that read private keys indeed correspond to the hashlock public keys
    check_prv_keys(&hashlock_prv_keys, vec![key3_a, key3_b]);

    // Send preimage + multisig path prv keys from the maker2users contracts
    send_preimage_and_prv_keys(preimage, vec![prv_key4, prv_key6], &mut new_writers).await;
    println!("Maker2users contract PrvKeys ----> Users (X/Y)");

    // Users can now redeem their funds from the respective maker2user contract

    // Receive users2maker contract keys
    let prv_keys = read_prv_keys(&mut old_readers).await;
    check_prv_keys(&prv_keys, vec![key1_a, key1_b]);
    println!("Users2maker contract PrvKeys <---- Users (A/B)");

    // Maker can now spend from:
    let _prv_desc = users2maker_prv_desc
        .replace(&key1_a.to_string(), &prv_keys[0].to_string())
        .replace(&key1_b.to_string(), &prv_keys[1].to_string());

    let total_received = funding_final.unsigned_tx.output[0].value;
    let profit = total_received - total_spent;

    println!("\nSuccesful JoinSwap! Maker earned {profit} sats");
}

async fn send_preimage_and_prv_keys(
    preimage: [u8; 32],
    prv_keys: Vec<PrivateKey>,
    writers: &mut Vec<WriteHalf<TcpStream>>,
) {
    assert_eq!(prv_keys.len(), writers.len());
    let serialized_preimage = serde_json::to_string(&preimage).unwrap();

    for (key, mut writer) in prv_keys.iter().zip(writers) {
        send_message(serialized_preimage.clone(), &mut writer).await;
        send_message(key.to_string(), &mut writer).await;
    }
}

async fn read_prv_keys(
    readers: &mut Vec<BufReader<ReadHalf<TcpStream>>>
) -> Vec<PrivateKey> {
    assert_eq!(readers.len(), 2);

    let mut prv_keys = Vec::new();
    for mut reader in readers {
        let prv_key_str = read_message(&mut reader).await;
        prv_keys.push(PrivateKey::from_str(prv_key_str.trim()).unwrap());
    }

    prv_keys
}

async fn send_second_contract_data(
    maker_keys: Vec<&[PublicKey; 2]>,
    txids: Vec<Txid>,
    writers: &mut Vec<WriteHalf<TcpStream>>,
) {
    assert_eq!(maker_keys.len(), txids.len());
    assert_eq!(maker_keys.len(), writers.len());

    for ((key_pair, txid), mut writer) in maker_keys.iter().zip(txids).zip(writers) {
        let keys_str = format!("{},{}", key_pair[0], key_pair[1]);

        send_message(keys_str, &mut writer).await;
        send_message(txid.to_string(), &mut writer).await;
    }
}

// The amount sent is fixed for now.
fn build_second_funding(wallet: &Wallet<AnyDatabase>, pub_desc: &Descriptor<PublicKey>) -> Psbt {
    let mut tx_builder = wallet.build_tx();

    tx_builder.add_recipient(pub_desc.script_pubkey(), 45000);

    let (psbt, _) = tx_builder.finish().unwrap();

    psbt
}

fn gen_hash() -> ([u8; 32], sha256::Hash) {
    let mut rng = thread_rng();
    let mut bytes = [0u8; 32];
    rng.fill(&mut bytes[..]);

    let hash = sha256::Hash::hash(&bytes);

    (bytes, hash)
}

async fn read_second_user_data(reader: &mut BufReader<ReadHalf<TcpStream>>) -> (PublicKey, PublicKey) {
    let keys = read_contract_keys(reader, 2).await;

    (keys[0], keys[1])
}

async fn send_psbt(psbt: &Psbt, writers: &mut Vec<WriteHalf<TcpStream>>) {
    let serialized_psbt = serde_json::to_string(&psbt).unwrap();

    for mut writer in writers {
        send_message(serialized_psbt.to_string(), &mut writer).await;
    }
}

async fn send_contract_data(
    keys: &[PublicKey; 9],
    hash: sha256::Hash,
    funding: &Psbt,
    refund: &Psbt,
    writers: &mut Vec<WriteHalf<TcpStream>>,
) {
    let serialized_funding = serde_json::to_string(&funding).unwrap();
    let serialized_refund = serde_json::to_string(&refund).unwrap();

    let keys_str = format!(
        "{},{},{},{},{},{},{},{},{}",
        keys[0], keys[1], keys[2], keys[3], keys[4], keys[5], keys[6], keys[7], keys[8]);

    for mut writer in writers {
        send_message(keys_str.clone(), &mut writer).await;
        send_message(hash.to_string(), &mut writer).await;
        send_message(serialized_funding.clone(), &mut writer).await;
        send_message(serialized_refund.clone(), &mut writer).await;
    }
}

async fn read_user_data(
    reader: &mut BufReader<ReadHalf<TcpStream>>
) -> ((PublicKey, PublicKey, PublicKey), WeightedUtxo, Address) {
    let keys = read_contract_keys(reader, 3).await;
    let weighted = read_utxo_data(reader).await;
    let addr = read_refund(reader).await;

    ((keys[0], keys[1], keys[2]), weighted, addr)
}

async fn read_and_combine_psbt(
    readers: &mut Vec<BufReader<ReadHalf<TcpStream>>>,
    txid: Option<Txid>,
) -> Psbt {
    assert_eq!(readers.len(), 2);

    let mut signed_psbts = Vec::new();
    for mut reader in readers {
        let signed_psbt = read_psbt(&mut reader, txid).await;
        signed_psbts.push(signed_psbt);
    }
    let mut final_psbt = signed_psbts[0].clone();
    final_psbt.combine(signed_psbts[1].clone()).unwrap();

    final_psbt
}

async fn accept_connection(listener: &TcpListener) -> (BufReader<ReadHalf<TcpStream>>, WriteHalf<TcpStream>) {
    let (socket, _) = listener.accept().await.unwrap();
    let (reader, writer) = split(socket);
    let reader = BufReader::new(reader);

    (reader, writer)
}

async fn read_utxo_data(reader: &mut BufReader<ReadHalf<TcpStream>>) -> WeightedUtxo {
    let mut line = read_message(reader).await;
    let desc = Descriptor::<PublicKey>::from_str(&line.trim()).unwrap();

    line = read_message(reader).await;
    let outpoint = OutPoint::from_str(&line.trim()).unwrap();

    line = read_message(reader).await;
    let psbt_in: psbt::Input = serde_json::from_str(&line.trim()).unwrap();

    assert_eq!(
        psbt_in.witness_utxo.as_ref().unwrap().script_pubkey,
        desc.script_pubkey(),
        "The descriptor needs to match the utxo");

    WeightedUtxo {
        satisfaction_weight: desc.max_satisfaction_weight().unwrap(),
        utxo: Utxo::Foreign { outpoint, psbt_input: Box::new(psbt_in) },
    }
}

async fn read_refund(reader: &mut BufReader<ReadHalf<TcpStream>>) -> Address {
    let line = read_message(reader).await;

    Address::from_str(&line.trim()).unwrap()
}