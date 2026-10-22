//! `spilman-payer` — **a TEST DRIVER**, not a product feature.
//!
//! It performs the full payer half of the channel protocol against a *running*
//! payee, so the physical-router E2E can exercise the protocol over HTTP instead of
//! only in-process:
//!
//! ```text
//! 1. GET  /spilman/params    read the payee's mint + rate
//! 2. (local)                 open a channel against that mint, fund it
//! 3. POST /spilman/channel   register the funding record with the payee
//! 4. POST /spilman/payment   sign a balance update and pay
//! 5. GET  /spilman/status    read back what the payee credited
//! ```
//!
//! # Why it is an example, not a `[[bin]]`
//!
//! It needs the in-process `cdk-spilman-test-mint`, which is a **dev-dependency**.
//! A `[[bin]]` cannot use dev-dependencies; an example can. Making it a bin would
//! force the test mint into the dependency graph of every shipped artifact — a
//! router image carrying a fake mint is exactly the kind of thing that later gets
//! switched on by accident. It is additionally behind `required-features =
//! ["payer-driver"]`, so a plain `cargo build` / `cargo test` neither builds nor
//! installs it.
//!
//! # Running it
//!
//! ```text
//! # terminal 1 — the payee, with the rail on
//! cargo run --release
//! cargo run --example spilman-payer --features payer-driver -- --payee http://127.0.0.1:8080
//! ```
//!
//! Every step prints its outcome; the process exits non-zero on the first failure.
//! That is the whole contract: a driver that printed a trace and exited 0 would
//! make a red E2E look green.
//!
//! # What it deliberately does not do
//!
//! No retries, no fallbacks, no partial success. Each step is a precondition for the
//! next (a channel that did not open cannot be registered; a payment the payee
//! refused must not be followed by a "settled" line), so continuing past a failure
//! would produce a log that reads like a run and isn't one.

use cdk_spilman::{
    build_cashu_b_token, parse_keyset_info_from_json, ConfigurableClientHost, SpilmanClientBridge,
};
use cdk_spilman_test_mint::{serve_existing_mint_with_shutdown, TestMintConfig, TestMintHelper};
use cashu::nuts::SecretKey;
use tollgate_module_basic_rust::spilman::payer::{
    self, ChannelOpening, DEFAULT_CHANNEL_LIFETIME_SECS,
};
use tollgate_module_basic_rust::spilman::registration::ChannelRegistrationRequest;
use tollgate_module_basic_rust::spilman::payer_driver::PayeeClient;
use tollgate_module_basic_rust::spilman::SpilmanError;

/// Capacity, in sats, of the channel this driver opens.
///
/// One sat is 22_021_000 bytes at the parity rate, so this is far more than the
/// driver spends; the interesting number is the *balance it signs*, not the ceiling.
const CHANNEL_CAPACITY: u64 = 64;

/// How much of the channel the driver actually spends, in raw channel units.
const PAYMENT_RAW_UNITS: u64 = 1;

/// One request's timeout against the payee — see `PayableClient`'s own default.
const EXAMPLE_PAYEE_URL: &str = "http://127.0.0.1:8080";


/// Print a step's outcome in a shape a log can be grepped for.
fn step(n: u8, what: &str, outcome: &str) {
    println!("[{n}/5] {what}: {outcome}");
}

/// Emit a MACHINE-readable marker line.
///
/// The human `step()` lines above are for a person reading a log; these are the
/// contract the physical-router lane parses
/// (`spilman/spilman_router_lane.py`, `RE_OPEN_OK` / `RE_SETTLE_OK` / `RE_EXIT`).
/// Both are printed: a marker that replaced the human line would make the lane
/// pass while making the failure harder for a person to read, and the lane's own
/// rule is that a driver which exists but does not emit these is a FAIL, not a
/// SKIP — so the two halves have to agree on an exact shape.
///
/// The shapes are fixed by the lane's regexes:
///   PAYER_OPEN_OK channel_id=<hex>
///   PAYER_SETTLE_OK channel_id=<hex> balance=<n> payee_credited=<n>
///   PAYER_EXIT=<n>
fn marker(line: &str) {
    println!("{line}");
}

/// The exit code the driver ends with, mirrored as a marker line.
///
/// The lane reads the log text over a serial console, where the process's real
/// exit status is not visible; `PAYER_EXIT=<n>` is how it learns the outcome.
/// Printing it on every path — including the failure paths — is what keeps a
/// failed run from looking like a truncated one.
fn exit_with(code: i32) -> ! {
    marker(&format!("PAYER_EXIT={code}"));
    std::process::exit(code);
}

/// Fail the run: print why and exit non-zero.
///
/// `panic = "abort"` is set for release, so a panic in this driver would abort
/// without unwinding — no destructors, and (depending on the harness) an exit code
/// that does not distinguish "the payee refused me" from "the driver crashed". An
/// explicit non-zero exit is the honest signal.
fn fail(what: &str, err: &SpilmanError) -> ! {
    eprintln!("FAILED at {what}: {err}");
    marker(&format!("PAYER_FAILED={what}"));
    exit_with(1);
}

/// A mint networking client for one leg of the protocol.
///
/// `ReqwestClientNetworking` bridges the sync trait to async reqwest with
/// `block_in_place`, so it needs a multi-threaded runtime — which `main` provides.
fn mint_networking() -> Result<cdk_spilman::ReqwestClientNetworking, SpilmanError> {
    cdk_spilman::ReqwestClientNetworking::new(std::time::Duration::from_secs(20))
        .map_err(|e| SpilmanError::Config(format!("build the mint networking client: {e}")))
}

/// Parse `--payee <url>`; anything else is a usage error.
fn parse_args() -> String {
    let mut args = std::env::args().skip(1);
    let mut payee: Option<String> = None;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--payee" => payee = args.next(),
            "--help" | "-h" => {
                println!("usage: spilman-payer --payee <base-url>");
                println!();
                println!("  --payee <url>   the running module's base URL, e.g. {EXAMPLE_PAYEE_URL}");
                exit_with(0);
            }
            other => {
                eprintln!("unexpected argument: {other}");
                eprintln!("usage: spilman-payer --payee <base-url>");
                exit_with(2);
            }
        }
    }
    match payee {
        Some(url) if !url.trim().is_empty() => url,
        _ => {
            eprintln!("usage: spilman-payer --payee <base-url>");
            exit_with(2);
        }
    }
}

#[tokio::main(flavor = "multi_thread", worker_threads = 4)]
async fn main() {
    // `ReqwestClientNetworking` bridges sync->async with `block_in_place`, so a
    // multi-thread runtime is required; `multi_thread` above is not decoration.
    let payee_url = parse_args();

    let tempdir = match tempfile::tempdir() {
        Ok(dir) => dir,
        Err(e) => {
            eprintln!("FAILED to create a working directory: {e}");
            exit_with(1);
        }
    };

    // ── STEP 1: what does the payee want? ───────────────────────────────────
    let payee = match PayeeClient::new(&payee_url) {
        Ok(client) => client,
        Err(e) => fail("build the payee client", &e),
    };
    let params = match payee.params().await {
        Ok(params) => params,
        Err(e) => fail("GET /spilman/params", &e),
    };
    let advertised_mint = params
        .get("mint_url")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("<none>")
        .to_string();
    step(
        1,
        "GET /spilman/params",
        &format!(
            "mint_url={advertised_mint} in_bytes_per_millisat={}",
            params
                .get("in_bytes_per_millisat")
                .map(|v| v.to_string())
                .unwrap_or_else(|| "<none>".to_string())
        ),
    );

    // ── STEP 2: open and fund a real channel against a real mint ────────────
    //
    // The mint is in-process (a FakeWallet-backed real `cdk::Mint`), so the driver
    // needs no external service. It is served over HTTP as well as handed to the
    // payer's bridge, because the *payee* must be able to name the same mint: a
    // channel funded at `https://test-mint` is only verifiable if the record the
    // payee stores says `https://test-mint` too.
    let helper = match TestMintHelper::new().await {
        Ok(helper) => helper,
        Err(e) => {
            eprintln!("FAILED to start the in-process test mint: {e}");
            exit_with(1);
        }
    };
    let keyset_info_json = match helper.keyset_info_json() {
        Ok(json) => json,
        Err(e) => {
            eprintln!("FAILED to read the test mint's keyset info: {e}");
            exit_with(1);
        }
    };

    // Bind an ephemeral port so two drivers can run at once.
    let listener = match tokio::net::TcpListener::bind(("127.0.0.1", 0)).await {
        Ok(listener) => listener,
        Err(e) => {
            eprintln!("FAILED to bind the test mint: {e}");
            exit_with(1);
        }
    };
    let mint_port = match listener.local_addr() {
        Ok(addr) => addr.port(),
        Err(e) => {
            eprintln!("FAILED to read the test mint's address: {e}");
            exit_with(1);
        }
    };
    drop(listener); // the server below re-binds it; this was only to pick a port
    let mint_url = format!("http://127.0.0.1:{mint_port}");

    if advertised_mint != "<none>" && advertised_mint != mint_url {
        eprintln!(
            "WARNING: the payee advertises mint {advertised_mint}, but this driver funds at \
             {mint_url}. The payee stores whatever it is told and refuses payments it cannot \
             verify, so this run proves the WIRE works, not that the payee trusts this mint."
        );
    }

    // The mint has to actually listen, because the payer's funding swap goes over
    // HTTP via `ReqwestClientNetworking` (not the in-memory shortcut) — this driver
    // is meant to look like a real payer, so the mint leg is real too.
    let mint_for_server = helper.mint();
    let server_config = TestMintConfig::for_port(mint_port);
    let server = tokio::spawn(async move {
        serve_existing_mint_with_shutdown(mint_for_server, server_config, async {
            // Runs until the driver exits; the process teardown ends it.
            let _ = tokio::signal::ctrl_c().await;
        })
        .await
    });

    // Give the listener a moment to come up, then prove it did rather than
    // sleeping and hoping.
    let mint_info_url = format!("{mint_url}/v1/info");
    let mut ready = false;
    for _ in 0..50 {
        match reqwest::get(&mint_info_url).await {
            Ok(resp) if resp.status().is_success() => {
                ready = true;
                break;
            }
            _ => tokio::time::sleep(std::time::Duration::from_millis(100)).await,
        }
    }
    if !ready {
        eprintln!("FAILED: the in-process test mint never answered {mint_info_url}");
        server.abort();
        exit_with(1);
    }

    let sender_secret = SecretKey::generate();
    let receiver_secret = SecretKey::generate();

    let payer_db = tempdir.path().join("payer.sqlite");
    let mut client_host = match ConfigurableClientHost::open_sqlite(
        payer_db.to_str().unwrap_or("payer.sqlite"),
    ) {
        Ok(host) => host,
        Err(e) => {
            eprintln!("FAILED to open the payer store: {e}");
            server.abort();
            exit_with(1);
        }
    };
    client_host.add_key(sender_secret.clone());

    // Two mint clients, on purpose. `SpilmanClientBridge` consumes its networking by
    // value and exposes no accessor (see `spilman::payer`'s module docs), and a mint
    // client is stateless configuration — so step 3 gets its own rather than the
    // driver pretending one object can be shared.
    let bridge_networking = match mint_networking() {
        Ok(networking) => networking,
        Err(e) => {
            server.abort();
            eprintln!("FAILED to build the mint networking client: {e}");
            exit_with(1);
        }
    };
    let swap_networking = match mint_networking() {
        Ok(networking) => networking,
        Err(e) => {
            server.abort();
            eprintln!("FAILED to build the mint networking client: {e}");
            exit_with(1);
        }
    };
    let client = SpilmanClientBridge::new(client_host, bridge_networking);

    let proofs = match helper.mint_proofs(CHANNEL_CAPACITY * 2).await {
        Ok(proofs) => proofs,
        Err(e) => {
            eprintln!("FAILED to mint test proofs: {e}");
            server.abort();
            exit_with(1);
        }
    };
    let proofs_json = match serde_json::to_string(&proofs) {
        Ok(json) => json,
        Err(e) => {
            eprintln!("FAILED to serialize test proofs: {e}");
            server.abort();
            exit_with(1);
        }
    };
    let token = match build_cashu_b_token(&mint_url, "sat", &proofs_json) {
        Ok(token) => token,
        Err(e) => {
            eprintln!("FAILED to build the funding token: {e}");
            server.abort();
            exit_with(1);
        }
    };

    let expiry = payer::now_seconds() + DEFAULT_CHANNEL_LIFETIME_SECS;
    let mut opening = match ChannelOpening::prepare(
        &client,
        &token,
        &receiver_secret.public_key().to_hex(),
        &sender_secret.public_key().to_hex(),
        expiry,
        &keyset_info_json,
        64,
    ) {
        Ok(opening) => opening,
        Err(e) => {
            server.abort();
            fail("prepare the channel open", &e)
        }
    };
    let channel_id = opening.channel_id().to_string();
    let capacity = opening.capacity();

    // The two-phase open, in the only order that protects money: persist the
    // restore material, THEN swap.
    if let Err(e) = opening.save_opening(&client) {
        server.abort();
        fail("persist the opening record", &e);
    }
    if let Err(e) = opening.submit_funding_swap(&swap_networking) {
        server.abort();
        fail("submit the funding swap", &e);
    }
    let completed = match opening.finish_open(&client) {
        Ok(completed) => completed,
        Err(e) => {
            server.abort();
            fail("complete the channel open", &e)
        }
    };

    // The registration payload is built from upstream's own `Payment`, so the bytes
    // the payee receives are the bytes cdk-spilman produced.
    let registration_payment = match client.sign_channel_registration(&channel_id) {
        Ok(payment) => payment,
        Err(e) => {
            server.abort();
            eprintln!("FAILED to sign the channel registration: {e}");
            exit_with(1);
        }
    };
    let keyset_id = match parse_keyset_info_from_json(&keyset_info_json) {
        Ok(info) => info.keyset_id,
        Err(e) => {
            server.abort();
            eprintln!("FAILED to parse the keyset info this driver just fetched: {e}");
            exit_with(1);
        }
    };
    // `ChannelRegistrationRequest::from_registration` is the only translation from
    // upstream's `Payment` to the payee's wire body, so the params and funding proofs
    // the payee receives are the exact bytes cdk-spilman produced — no re-serialisation.
    //
    // `Payment` carries neither the ECDH channel secret nor the keyset blob, so the
    // payer adds both here: it derived the secret at open time, and the payee has no
    // way to recover it from the channel id alone.
    let registration = match ChannelRegistrationRequest::from_registration(&registration_payment)
        .and_then(|req| {
            req.with_channel_secret(&sender_secret, &receiver_secret.public_key().to_hex())
        }) {
        Ok(req) => req.with_keyset_info_json(keyset_info_json.clone()),
        Err(e) => {
            server.abort();
            fail("build the registration payload", &e)
        }
    };

    // `finish_open` returns the mint's own view of the channel; the funding proofs
    // live on the opening (they are the restore material), so both are reported.
    let funding_proofs_len = opening.funding_proofs_json().map(str::len).unwrap_or(0);
    step(
        2,
        "open + fund a channel",
        &format!(
            "channel_id={channel_id} capacity={capacity} mint={} unit={} keyset={} \
             funding_proofs={funding_proofs_len}B mint_channel_id={}",
            completed.mint_url, completed.unit, completed.keyset_id, completed.channel_id
        ),
    );
    if completed.channel_id != channel_id {
        eprintln!(
            "FAILED: the mint completed a channel named {}, but the payer prepared {channel_id}",
            completed.channel_id
        );
        server.abort();
        marker(&format!("PAYER_FAILED=mint-channel-id-mismatch channel_id={channel_id}"));
        exit_with(1);
    }

    // The lane's E assertion: the channel is open and funded. Emitted only after the
    // id-mismatch check above, so a marker means "open AND agrees with the mint".
    marker(&format!(
        "PAYER_OPEN_OK channel_id={channel_id} capacity={capacity}"
    ));

    // ── STEP 3: register it with the running payee ──────────────────────────
    let outcome = match payee.register_channel(&registration).await {
        Ok(outcome) => outcome,
        Err(e) => {
            server.abort();
            fail("POST /spilman/channel", &e)
        }
    };
    step(
        3,
        "POST /spilman/channel",
        &format!("outcome={outcome} channel_id={channel_id} keyset={keyset_id}"),
    );

    // ── STEP 4: sign and submit a payment ───────────────────────────────────
    let payment = match payer::step_payment(&client, &channel_id, PAYMENT_RAW_UNITS) {
        Ok(payment) => payment,
        Err(e) => {
            server.abort();
            fail("sign a balance update", &e)
        }
    };
    let paid = match payee
        .submit_payment(&channel_id, payment.balance, &payment.signature)
        .await
    {
        Ok(outcome) => outcome,
        Err(e) => {
            server.abort();
            fail("POST /spilman/payment", &e)
        }
    };
    step(
        4,
        "POST /spilman/payment",
        &format!("outcome={paid} balance={}", payment.balance),
    );

    // ── STEP 5: what did the payee actually credit? ─────────────────────────
    //
    // The assertion that makes this a test rather than a demo: the payee must report
    // the balance the payer signed. A 200 on step 4 is the payee saying it accepted;
    // this is the payee saying what it *holds*.
    let status = match payee.status(&channel_id).await {
        Ok(status) => status,
        Err(e) => {
            server.abort();
            fail("GET /spilman/status", &e)
        }
    };
    let credited = status.get("balance").and_then(serde_json::Value::as_u64);
    step(
        5,
        "GET /spilman/status",
        &format!(
            "balance={} decision={}",
            credited
                .map(|b| b.to_string())
                .unwrap_or_else(|| "<null>".to_string()),
            status
                .get("decision")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("<none>")
        ),
    );

    server.abort();

    if credited != Some(PAYMENT_RAW_UNITS) {
        eprintln!(
            "FAILED: the payee reports balance {credited:?}, but the payer signed \
             {PAYMENT_RAW_UNITS}. The channel protocol did not complete."
        );
        marker(&format!(
            "PAYER_FAILED=payee-held-a-different-balance channel_id={channel_id} signed={PAYMENT_RAW_UNITS} payee_credited={}",
            credited.map(|b| b.to_string()).unwrap_or_else(|| "<null>".to_string())
        ));
        exit_with(1);
    }

    // The lane's F/G assertions: the payment completed AND the payee holds exactly the
    // balance the payer signed. `payee_credited` is the payee's own read-back, not the
    // driver's assumption — a 200 on step 4 would only mean "accepted".
    marker(&format!(
        "PAYER_SETTLE_OK channel_id={channel_id} balance={PAYMENT_RAW_UNITS} payee_credited={}",
        credited.unwrap_or(0)
    ));

    println!();
    println!(
        "OK: channel {channel_id} registered, paid {PAYMENT_RAW_UNITS} raw unit(s), and the \
         payee holds exactly that balance."
    );
    drop(helper); // keep the mint (and its task) alive until after the last request
    exit_with(0);
}
