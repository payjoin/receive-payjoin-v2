use std::{collections::HashMap, str::FromStr};

use bitcoincore_rpc::RpcApi;
use payjoin::bitcoin::{psbt::Psbt, Amount, OutPoint};
use url::Url;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let ohttp_relay = Url::parse("https://pj.bobspacebkk.com")?;
    let payjoin_directory = Url::parse("https://payjo.in")?;

    // Fetch keys using HTTP CONNECT method
    let ohttp_keys =
        payjoin::io::fetch_ohttp_keys(ohttp_relay.clone(), payjoin_directory.clone()).await?;
    println!("OHTTP keys: {:?}", ohttp_keys);

    // Initialize A Payjoin Receive Session
    let bitcoind_rpc = "http://localhost:38332/wallet/receive";
    let bitcoind_cookie = "/Users/dan/Library/Application Support/Bitcoin/signet/.cookie";
    let bitcoind_cookie = bitcoincore_rpc::Auth::CookieFile(bitcoind_cookie.into());
    let bitcoind = bitcoincore_rpc::Client::new(bitcoind_rpc, bitcoind_cookie)?;
    let address = bitcoind.get_new_address(None, None)?;
    let mut session = payjoin::receive::v2::SessionInitializer::new(
        address.assume_checked(),
        payjoin_directory,
        ohttp_keys,
        ohttp_relay,
        std::time::Duration::from_secs(600),
    );
    let (req, ctx) = session.extract_req()?;
    let http = reqwest::Client::new();
    let res = http
        .post(req.url)
        .body(req.body)
        .header("Content-Type", payjoin::V2_REQ_CONTENT_TYPE)
        .send()
        .await?;
    let mut session = session.process_res(res.bytes().await?.to_vec().as_slice(), ctx)?;

    // Listen on a Bitcoin URI with payjoin support
    let uri = session
        .pj_uri_builder()
        .amount(payjoin::bitcoin::Amount::from_sat(88888))
        .build();
    println!("Payjoin URI:\n{}", uri);
    let proposal = loop {
        let (req, ctx) = session.extract_req()?;
        let res = http
            .post(req.url)
            .body(req.body)
            .header("Content-Type", payjoin::V2_REQ_CONTENT_TYPE)
            .send()
            .await?;
        match session.process_res(res.bytes().await?.to_vec().as_slice(), ctx)? {
            Some(proposal) => {
                break proposal;
            }
            None => {
                continue;
            }
        }
    };

    // validate proposal using the check methods

    let mut payjoin = proposal
        .check_broadcast_suitability(None, |tx| {
            Ok(bitcoind
                .test_mempool_accept(&[payjoin::bitcoin::consensus::encode::serialize_hex(&tx)])
                .unwrap()
                .first()
                .unwrap()
                .allowed)
        })
        .expect("Payjoin proposal should be broadcastable")
        .check_inputs_not_owned(|input| {
            let address =
                payjoin::bitcoin::Address::from_script(&input, payjoin::bitcoin::Network::Signet)
                    .unwrap();
            Ok(bitcoind
                .get_address_info(&address)
                .unwrap()
                .is_mine
                .unwrap())
        })
        .expect("Receiver should not own any of the inputs")
        .check_no_mixed_input_scripts()
        .expect("No mixed input scripts")
        .check_no_inputs_seen_before(|_| Ok(false))
        .expect("No inputs seen before")
        .identify_receiver_outputs(|output_script| {
            let address = payjoin::bitcoin::Address::from_script(
                &output_script,
                payjoin::bitcoin::Network::Signet,
            )
            .unwrap();
            Ok(bitcoind
                .get_address_info(&address)
                .unwrap()
                .is_mine
                .unwrap())
        })
        .expect("Receiver should have at least one output");

    // Augment the Proposal to Make a Batched Transaction
    let available_inputs = bitcoind.list_unspent(None, None, None, None, None)?;
    let candidate_inputs: HashMap<Amount, OutPoint> = available_inputs
        .iter()
        .map(|i| {
            (
                i.amount,
                OutPoint {
                    txid: i.txid,
                    vout: i.vout,
                },
            )
        })
        .collect();
    let selected_outpoint = payjoin.try_preserving_privacy(candidate_inputs).unwrap();
    let selected_utxo = available_inputs
        .iter()
        .find(|i| i.txid == selected_outpoint.txid && i.vout == selected_outpoint.vout)
        .unwrap();
    let txo_to_contribute = payjoin::bitcoin::TxOut {
        value: selected_utxo.amount.to_sat(),
        script_pubkey: selected_utxo.script_pub_key.clone(),
    };
    let outpoint_to_contribute = OutPoint {
        txid: selected_utxo.txid,
        vout: selected_utxo.vout,
    };
    payjoin.contribute_witness_input(txo_to_contribute, outpoint_to_contribute);

    let mut payjoin = payjoin.finalize_proposal(
        |psbt| {
            Ok(bitcoind
                .wallet_process_psbt(&psbt.to_string(), None, None, Some(true))
                .map(|res| Psbt::from_str(&res.psbt).unwrap())
                .unwrap())
        },
        Some(payjoin::bitcoin::FeeRate::MIN),
    )?;

    let (req, ctx) = payjoin.extract_v2_req()?;
    let res = http
        .post(req.url)
        .body(req.body)
        .header("Content-Type", payjoin::V2_REQ_CONTENT_TYPE)
        .send()
        .await?;
    payjoin.process_res(res.bytes().await?.to_vec(), ctx)?;
    let payjoin_psbt = payjoin.psbt().clone();
    println!(
        "response successful. Watch mempool for successful payjoin. TXID: {}",
        payjoin_psbt.extract_tx().clone().txid()
    );
    Ok(())
}
