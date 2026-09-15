//! The commands: decode, classify, reproduce, calibrate, follow.

use std::sync::Arc;

use birdai_amm::{
    Direction, MAX_SQRT_PRICE, MIN_SQRT_PRICE, Q64, sqrt_price_at_tick, swap::SwapResult,
};
use birdai_resolve::fixture::Fixtures;
use birdai_state::StateManager;
use birdai_tick::{SizeSkew, Ticks};
use birdai_venue::{AnyVenue, CetusClmm, Classifier, Venue, price_state_change, venue_kind_of};
use move_core_types::account_address::AccountAddress;
use prometheus::Registry;
use sui_indexer_alt_framework::ingestion::{
    ClientArgs, IngestionConfig, IngestionService, ingestion_client::IngestionClientArgs,
    streaming_client::StreamingClientArgs,
};
use sui_types::{
    base_types::{ObjectID, SequenceNumber},
    digests::TransactionDigest,
    effects::TransactionEffectsAPI,
    storage::ObjectKey,
    transaction::TransactionDataAPI,
};
use url::Url;

use crate::{
    constants::{
        POOL_A, POOL_A_FEE_RATE, POOL_A_POST_VERSION, POOL_A_PRE_VERSION, POOL_A_TICK_SPACING,
        POOL_A_TYPE, POOL_B, POOL_C, POOL_SCRIPT_PACKAGE, TX_T, TX_T_AMOUNT_IN, TX_T_AMOUNT_OUT,
        TX_T_CHECKPOINT, object_id,
    },
    session::Session,
    ticks::load_tick_nodes,
};

/// Rule of thumb for headings.
fn rule(title: &str) {
    println!("\n══ {title} {}", "═".repeat(78_usize.saturating_sub(title.len())));
}

/// Decode objects A, B and C, plus one tick child of A.
pub(crate) async fn decode(session: &Session) -> eyre::Result<()> {
    for (label, id, note) in [
        ("A", POOL_A, "Cetus CLMM pool, the trading venue"),
        ("B", POOL_B, "Volo liquid-staking pool, a vault"),
        ("C", POOL_C, "Navi lending storage, a ledger"),
    ] {
        rule(&format!("object {label} — {note}"));
        let decoded = session.decode(object_id(id)?, None).await?;
        println!("id      {}", decoded.object.id().to_canonical_string(true));
        println!("version {}", decoded.object.version().value());
        println!("digest  {}", decoded.object.digest());
        println!("owner   {:?}", decoded.object.owner());
        println!("type    {}", decoded.tag.to_canonical_string(true));
        let bytes = decoded.object.data.try_as_move().map_or(0, |object| object.contents().len());
        println!("bcs     {bytes} bytes");
        println!();
        for line in decoded.dump.lines() {
            println!("{line}");
        }
    }

    rule("object A — tick child at the current tick");
    let pool = load_pool(session, None).await?;
    let (index, skew) = load_tick_index(session, &pool).await?;
    println!(
        "ticks node UID  {}  (an *inner* UID, not the pool object)",
        pool.ticks.node_uid.to_canonical_string(true)
    );
    println!("ticks declared  {}", pool.ticks.size);
    println!("ticks decoded   {}", index.len());
    match skew {
        None => println!("size check      exact: the children match the pool's own count"),
        Some(skew) => println!(
            "size check      SKEWED by {}: the pool says {} but {} children exist now, \
             because dynamic fields can only be listed as of the present",
            skew.delta(),
            skew.declared,
            skew.observed
        ),
    }
    println!(
        "price check     {}/{} nodes store exactly floor(1.0001^(t/2)·2^64); max |Δ| = {} ulp, \
         histogram {:?}",
        index.exact_price_nodes(),
        index.len(),
        index.max_price_deviation(),
        index.price_deviations()
    );
    println!("ticks head      {:?}", pool.ticks.head);
    println!("ticks level     {}/{}", pool.ticks.level, pool.ticks.max_level);
    println!("current tick    {}", pool.tick);

    let (below, above) = index.bracketing(pool.tick);
    for (label, node) in [("below", below), ("above", above)] {
        let Some(node) = node else { continue };
        println!("\nnearest initialised tick {label} {}:", pool.tick);
        println!("  score            {}", node.score);
        println!(
            "  tick index       {}   (score - 443636 = {})",
            node.index(),
            node.score as i64 - 443_636
        );
        println!("  sqrt_price       {}", node.tick.sqrt_price);
        println!("  liquidity_net    {}", node.tick.liquidity_net);
        println!("  liquidity_gross  {}", node.tick.liquidity_gross);
        println!("  nexts            {:?}", node.nexts);
        println!("  prev             {:?}", node.prev);
        println!(
            "  fee_growth_outside_a/b   {} / {}",
            node.tick.fee_growth_outside_a, node.tick.fee_growth_outside_b
        );
        println!("  rewards_growth_outside   {:?}", node.tick.rewards_growth_outside);
    }
    if let (Some(low), Some(high)) = (below, above) {
        println!(
            "\nactive tick range [{}, {})   spacing {}",
            low.index(),
            high.index(),
            pool.tick_spacing
        );
    }
    Ok(())
}

/// Load pool A and type it.
pub(crate) async fn load_pool(session: &Session, version: Option<u64>) -> eyre::Result<CetusClmm> {
    let decoded = session.decode(object_id(POOL_A)?, version).await?;
    let move_object = decoded
        .object
        .data
        .try_as_move()
        .ok_or_else(|| eyre::eyre!("pool A is not a Move object"))?;
    let tag = decoded.object.struct_tag().ok_or_else(|| eyre::eyre!("pool A has no type tag"))?;
    // ponytail: reuse the shape gate so same-named foreign pools fail as unrecognised.
    match birdai_venue::decode_venue(move_object.contents(), &tag, &decoded.layout)? {
        AnyVenue::Cetus(pool) => Ok(*pool),
        other => eyre::bail!("pool A decoded as {}", other.kind().label()),
    }
}

/// Load and validate pool A's tick index.
///
/// Returns the index together with the size skew, if any. A skew is expected whenever the pool
/// object is fetched at a historical version: dynamic fields can only be enumerated as of *now*,
/// so the children reflect the pool's current tick set while the metadata comes from the version
/// that was asked for. Every other invariant still holds, and the skew is reported rather than
/// hidden.
pub(crate) async fn load_tick_index(
    session: &Session,
    pool: &CetusClmm,
) -> eyre::Result<(Arc<Ticks>, Option<SizeSkew>)> {
    // The pool keeps trading while it is read: listing the tick children is paginated, and
    // fetching them is a later round trip, so a tick added or removed in between leaves a
    // snapshot whose link graph does not close (`DanglingLink` for a neighbour that was never
    // listed, or a fetch skipped by the source's missing-object retry). One fresh snapshot is
    // almost always consistent; a second failure means the pool is churning faster than it can
    // be read, and failing loudly is the correct answer.
    match load_tick_snapshot(session, pool).await {
        Ok(loaded) => Ok(loaded),
        Err(first) => {
            println!("note: tick snapshot inconsistent ({first:#}); retrying once");
            load_tick_snapshot(session, pool).await
        }
    }
}

async fn load_tick_snapshot(
    session: &Session,
    pool: &CetusClmm,
) -> eyre::Result<(Arc<Ticks>, Option<SizeSkew>)> {
    let nodes =
        load_tick_nodes(session.objects.as_ref(), session.layouts.as_ref(), &pool.ticks, |_| {})
            .await?;
    let (index, skew) = Ticks::from_children(Some(pool.ticks.node_uid), pool.ticks.size, nodes)?;
    index.validate_spacing(pool.tick_spacing)?;
    Ok((Arc::new(index), skew))
}

/// Apply the price-discovery test to A, B and C.
pub(crate) async fn classify(session: &Session) -> eyre::Result<()> {
    let classifier = Classifier::default();

    // Object A gets the behavioural probe too: two versions of the pool bracketing transaction T.
    rule("object A — Cetus CLMM pool");
    let before = load_pool(session, Some(POOL_A_PRE_VERSION)).await?;
    let after = load_pool(session, Some(POOL_A_POST_VERSION)).await?;
    let checkpoint = session.checkpoint(TX_T_CHECKPOINT).await?;
    let digest: TransactionDigest =
        TX_T.parse().map_err(|error| eyre::eyre!("bad transaction digest {TX_T}: {error}"))?;
    let transaction = checkpoint
        .transactions
        .iter()
        .find(|tx| tx.effects.transaction_digest() == &digest)
        .ok_or_else(|| eyre::eyre!("transaction {TX_T} is not in checkpoint {TX_T_CHECKPOINT}"))?;
    let inputs: Vec<&sui_types::object::Object> =
        transaction.input_objects(&checkpoint.object_set).collect();
    let change = price_state_change(&before, &after, &inputs, classifier.deny_set());
    println!("  input objects: {}", inputs.len());
    println!("  oracle-ish inputs: {:?}", change.oracle_inputs);
    println!("  coin_a about to be spent: {} -> {}", before.coin_a, after.coin_a);
    println!("  fee_rate {} ({} bps)", before.fee_rate, before.fee_rate / 100);
    println!("  what the chain actually called:");
    let mut observed = None;
    for (index, package, module, function) in transaction.transaction.move_calls() {
        let entry = classifier
            .entry_at(session.layouts.as_ref(), AccountAddress::from(*package), module, function)
            .await?;
        println!(
            "    [{index}] {}::{module}::{function} {}",
            package.to_canonical_string(true),
            match &entry {
                Some(entry) => format!(
                    "← inter-asset swap shape, asset legs on type parameters {:?}",
                    entry.asset_type_parameters
                ),
                None => String::from("(not an inter-asset swap shape)"),
            }
        );
        if entry.is_some() && observed.is_none() {
            observed = entry;
        }
    }
    if let Some(entry) = &observed {
        println!(
            "  the executed entry is `{}::{}`: ({}) -> ({})",
            entry.module,
            entry.function,
            entry.parameters.join(", "),
            entry.returns.join(", ")
        );
    }

    let decoded = session.decode(object_id(POOL_A)?, Some(POOL_A_POST_VERSION)).await?;
    let verdict = classifier
        .classify_object(
            session.layouts.as_ref(),
            &decoded.object,
            Some(&change),
            observed.as_ref(),
        )
        .await?;
    print!("{}", verdict.render());

    for (label, id) in [("object B — Volo NativePool", POOL_B), ("object C — Navi Storage", POOL_C)]
    {
        rule(label);
        let decoded = session.decode(object_id(id)?, None).await?;
        let verdict = classifier
            .classify_object(session.layouts.as_ref(), &decoded.object, None, None)
            .await?;
        print!("{}", verdict.render());
        if let Some(kind) = venue_kind_of(&decoded.tag) {
            println!("  kind: {} → {}", kind.label(), kind.has_on_chain_price_discovery());
        }
    }

    rule("summary");
    println!("A  TRADING VENUE with on-chain price discovery — all three probes pass:");
    println!("     · the entry the chain executed, `pool_script_v2::swap_b2a`, mutably borrows");
    println!("       `Pool<T0, T1>` and carries coin legs on both of its type parameters;");
    println!("     · across T the pool's own `current_sqrt_price` rose while `coin_b` rose and");
    println!("       `coin_a` fell, with no oracle among the {} input objects;", inputs.len());
    println!("     · neither the pool's type nor its package links a price feed.");
    println!();
    println!(
        "B  NOT a venue — `NativePool` has no type parameters, so no function can take two of"
    );
    println!(
        "     its own assets and give the other back; the SUI↔VSUI rate is an accounting ratio"
    );
    println!("     that moves with reward accrual, and the object holds no price state at all.");
    println!();
    println!(
        "C  NOT a venue — `Storage` has no type parameters either, so the same clause rejects"
    );
    println!(
        "     every entry unconditionally (`deposit`/`withdraw`/`borrow`/`repay` each move one"
    );
    println!("     asset against a share claim); the package statically links an oracle");
    println!(
        "     (`PriceOracle` reached from `lending`, `logic`, `calculator`, `dynamic_calculator`);"
    );
    println!(
        "     and the object holds no price state — its 155 bytes are versions, tables and counts."
    );
    Ok(())
}

/// Recompute transaction T's output from the pool state it consumed.
pub(crate) async fn reproduce(session: &Session) -> eyre::Result<()> {
    rule("transaction T");
    println!("digest     {TX_T}");
    println!("checkpoint {TX_T_CHECKPOINT}");

    // The pre-state is reached two independent ways — by version, and as the pool object inside
    // the checkpoint's deduplicated object set — and both must agree.
    let pool = load_pool(session, Some(POOL_A_PRE_VERSION)).await?;
    let direct = session.object_at(object_id(POOL_A)?, POOL_A_PRE_VERSION).await?;
    let checkpoint = session.checkpoint(TX_T_CHECKPOINT).await?;
    let from_checkpoint = checkpoint
        .object_set
        .get(&ObjectKey(object_id(POOL_A)?, SequenceNumber::from_u64(POOL_A_PRE_VERSION)))
        .ok_or_else(|| {
            eyre::eyre!(
                "checkpoint {TX_T_CHECKPOINT} does not carry pool A at version {POOL_A_PRE_VERSION}"
            )
        })?;
    eyre::ensure!(
        direct.digest() == from_checkpoint.digest(),
        "the pool fetched by version and the pool in the checkpoint disagree"
    );
    let digest: TransactionDigest =
        TX_T.parse().map_err(|error| eyre::eyre!("bad transaction digest {TX_T}: {error}"))?;
    let transaction = checkpoint
        .transactions
        .iter()
        .find(|tx| tx.effects.transaction_digest() == &digest)
        .ok_or_else(|| eyre::eyre!("transaction {TX_T} is not in checkpoint {TX_T_CHECKPOINT}"))?;
    println!(
        "pre-state cross-checked: by version and via the checkpoint ({} inputs)",
        transaction.input_objects(&checkpoint.object_set).count()
    );

    eyre::ensure!(pool.fee_rate == POOL_A_FEE_RATE, "unexpected fee rate");
    eyre::ensure!(pool.tick_spacing == POOL_A_TICK_SPACING, "unexpected tick spacing");
    let decoded = session.decode(object_id(POOL_A)?, Some(POOL_A_PRE_VERSION)).await?;
    eyre::ensure!(
        decoded.tag.to_canonical_string(true) == POOL_A_TYPE,
        "pool A's type is not the one this repository reproduces"
    );

    let (index, skew) = load_tick_index(session, &pool).await?;

    println!("\npre-state (pool version {POOL_A_PRE_VERSION})");
    println!("  coin_a (USDC)        {}", pool.coin_a);
    println!("  coin_b (SUI)         {}", pool.coin_b);
    println!("  liquidity        L   {}", pool.liquidity);
    println!("  current_sqrt_price S {}", pool.sqrt_price);
    println!(
        "  S / 2^64             {:.12}",
        pool.price_state().map_or(0.0, |s| s.sqrt_price_real())
    );
    println!("  current_tick_index   {}", pool.tick);
    println!("  fee_rate             {} ({} bps)", pool.fee_rate, pool.fee_rate / 100);
    println!("  tick_spacing         {}", pool.tick_spacing);

    let (below, above) = index.bracketing(pool.tick);
    println!("\nactive tick range (children read at their current version)");
    if let Some(node) = below {
        println!("  lower  tick {} @ {}", node.index(), node.tick.sqrt_price);
    }
    if let Some(node) = above {
        println!("  upper  tick {} @ {}", node.index(), node.tick.sqrt_price);
    }
    if let Some(skew) = skew {
        println!(
            "  note: the pool at this version declares {} ticks but {} children exist today \
             (skew {}), so this set is a superset of the one in force",
            skew.declared,
            skew.observed,
            skew.delta()
        );
    }
    println!(
        "  tick prices: {}/{} exactly match floor(1.0001^(t/2)·2^64), max |Δ| = {} ulp",
        index.exact_price_nodes(),
        index.len(),
        index.max_price_deviation()
    );

    let (net, fee) = birdai_amm::take_fee(TX_T_AMOUNT_IN, pool.fee_rate)?;
    println!("\nstep");
    println!("  amount_in            {TX_T_AMOUNT_IN}");
    println!("  fee                  {fee}   (expected {})", crate::constants::TX_T_FEE);
    println!("  amount_in_after_fee  {net}");
    let delta =
        birdai_amm::next_sqrt_price_up(pool.sqrt_price, pool.liquidity, net)? - pool.sqrt_price;
    let reached = pool.sqrt_price + delta;
    println!("  ΔS = floor(in·2^64 / L)");
    println!("                       {delta}");
    println!("  S' = S + ΔS          {reached}");

    // Quote twice. The first run uses no tick boundaries at all, which is what the spacing argument
    // below licenses; the second uses the live tick children, so the two must agree.
    let no_boundaries = Ticks::default();
    let result: SwapResult =
        pool.quote_exact_in(&no_boundaries, Direction::BtoA, TX_T_AMOUNT_IN)?;
    let with_children = pool.quote_exact_in(&index, Direction::BtoA, TX_T_AMOUNT_IN)?;

    println!("\nresult");
    println!("  amount_out           {}", result.amount_out);
    println!("  on chain             {TX_T_AMOUNT_OUT}");
    println!("  difference           {}", result.amount_out as i128 - TX_T_AMOUNT_OUT as i128);
    println!("  steps                {}", result.steps.len());
    println!(
        "  liquidity end        {} (unchanged: {})",
        result.liquidity_end,
        result.liquidity_end == pool.liquidity
    );
    println!("  sqrt price end       {}", result.sqrt_price_end);
    println!(
        "  quoted with- and without tick boundaries agree: {}",
        result.amount_out == with_children.amount_out
    );

    let crossing = above.is_some_and(|node| result.sqrt_price_end >= node.tick.sqrt_price);
    let spacing_proof = pool.stays_inside_current_range(Direction::BtoA, TX_T_AMOUNT_IN)?;
    println!("\nwhy one step, with no tick crossed");
    println!(
        "  the move is {:.3} ticks, and tick_spacing is {}",
        move_in_ticks(pool.sqrt_price, result.sqrt_price_end),
        pool.tick_spacing
    );
    println!(
        "  a move below tick_spacing cannot reach another initialised tick: {}",
        spacing_proof
    );
    println!("  and the price stays below the next boundary above: {}", !crossing);
    println!(
        "  → the quote is exact without needing the tick set at all, which matters because the \
         tick set can only be read at its current version"
    );

    eyre::ensure!(
        result.amount_out == TX_T_AMOUNT_OUT,
        "recomputed {} but the chain produced {}",
        result.amount_out,
        TX_T_AMOUNT_OUT
    );
    eyre::ensure!(
        result.amount_out == with_children.amount_out,
        "the two quotes disagree: {} vs {}",
        result.amount_out,
        with_children.amount_out
    );
    println!("\n✔ exact match");
    Ok(())
}

/// How far a square-root price moved, measured in ticks.
///
/// `tick = 2·ln(√P) / ln(1.0001)`, so the distance is `2·ln(S'/S) / ln(1.0001)`.
fn move_in_ticks(from: u128, to: u128) -> f64 {
    if from == 0 {
        return 0.0;
    }
    let ratio = to as f64 / from as f64;
    if ratio <= 0.0 {
        return 0.0;
    }
    2.0 * ratio.ln() / 1.000_1_f64.ln()
}

/// Show how the pool's fixed-point format is derived rather than assumed.
pub(crate) async fn calibrate(session: &Session) -> eyre::Result<()> {
    for version in [None, Some(POOL_A_PRE_VERSION)] {
        let pool = load_pool(session, version).await?;
        rule(&format!(
            "pool A at version {}",
            version.map_or_else(|| "latest".to_owned(), |v| v.to_string())
        ));
        let real = pool.sqrt_price as f64 / Q64 as f64;
        let grid = 1.000_1_f64.powf(f64::from(pool.tick) / 2.0);
        println!("current_sqrt_price   {}", pool.sqrt_price);
        println!("current_tick_index   {}", pool.tick);
        println!();
        println!(
            "The pool stores √P scaled by an unknown power of two. Dividing by the grid price at"
        );
        println!(
            "the tick it reports should leave exactly that power of two, up to the fraction of a"
        );
        println!("tick the price sits above its grid point:");
        println!();
        println!("  S / 1.0001^(tick/2) = {:.6}", pool.sqrt_price as f64 / grid);
        println!("  2^64                = {:.6}", Q64 as f64);
        println!(
            "  ratio               = {:.9}   → within one tick of 2^64 (max 1.00005)",
            pool.sqrt_price as f64 / grid / Q64 as f64
        );
        println!("  → the format is Q64.64; S/2^64 = {real:.15}");
        println!();
        println!("price = (S/2^64)^2   {:.6} raw B per raw A", real * real);
        println!("tick_at_sqrt_price   {}", birdai_amm::tick_at_sqrt_price(pool.sqrt_price)?);
        println!(
            "sqrt_price_at_tick   {}   (the grid price the reported tick starts at)",
            sqrt_price_at_tick(pool.tick)?
        );
    }
    println!("\nMIN_SQRT_PRICE {MIN_SQRT_PRICE}\nMAX_SQRT_PRICE {MAX_SQRT_PRICE}");
    Ok(())
}

/// Stream checkpoints and keep in-memory venue state current.
///
/// Uses `sui_indexer_alt_framework::ingestion`, which gives hybrid gRPC streaming plus
/// object-store backfill, chain-id checks, exponential backoff, adaptive concurrency and
/// per-subscriber backpressure — none of which this crate would want to reimplement. The pipeline
/// half of the framework is not used because it requires a database-backed store; the state layer
/// here is in-memory by design.
pub(crate) async fn follow(
    session: &Session,
    rpc_url: &str,
    from: u64,
    count: u64,
) -> eyre::Result<()> {
    rule("streaming checkpoints");
    let uri: http::Uri = rpc_url.parse()?;
    let args = ClientArgs {
        ingestion: IngestionClientArgs {
            rpc_api_url: Some(Url::parse(rpc_url)?),
            ..IngestionClientArgs::default()
        },
        streaming: StreamingClientArgs { streaming_url: Some(uri) },
    };
    let registry = Registry::new();
    let mut service =
        IngestionService::new(args, IngestionConfig::default(), Some("birdai"), &registry)?;
    let mut receiver = service.subscribe_bounded(16);
    // `Service::spawn` starts its tasks immediately, and dropping it aborts them.
    let _service = service.run(from..).await?;
    println!("streaming from checkpoint {from} (hybrid: stream, backfilled from the store)");

    let manager = StateManager::new();
    let mut applied = 0_u64;
    let mut seen = 0_u64;

    while let Some(envelope) = receiver.recv().await {
        seen += 1;
        let report =
            manager.apply_checkpoint(session.layouts.as_ref(), &envelope.checkpoint).await?;
        if report.venues_updated > 0 {
            applied += 1;
            println!(
                "cp {:<12} venues {} (+{} new, -{}), children +{}, packages {}, skipped {}, failed {}",
                report.checkpoint,
                report.venues_updated,
                report.venues_created,
                report.venues_removed,
                report.children_indexed,
                report.packages_observed,
                report.unrecognised,
                report.failures
            );
            // A freshly tracked pool carries its price state but no ticks, and without ticks it
            // cannot be quoted. Load them on first sight; a pool that churns faster than it can
            // be read is warned about rather than retried forever.
            for slot in manager.venues() {
                if slot.ticks.is_some() {
                    continue;
                }
                let AnyVenue::Cetus(pool) = &slot.venue else { continue };
                match load_tick_nodes(
                    session.objects.as_ref(),
                    session.layouts.as_ref(),
                    &pool.ticks,
                    |_| {},
                )
                .await
                {
                    Ok(nodes) => match manager.install_ticks(slot.id, &pool.ticks, nodes) {
                        Ok(ticks) => println!("  {} ticks {} loaded", slot.id, ticks.len()),
                        Err(error) => println!("  {} ticks failed: {error:#}", slot.id),
                    },
                    Err(error) => println!("  {} ticks failed: {error:#}", slot.id),
                }
            }
        }
        if applied >= count {
            break;
        }
    }
    if applied < count {
        // ponytail: a closed stream before `count` venue checkpoints is not success.
        tracing::warn!(
            "stream ended after {seen} checkpoints with only {applied} venue checkpoints (wanted {count})"
        );
    }

    rule("final state");
    let stats = manager.stats();
    println!("checkpoints seen  {seen}");
    println!("venues tracked    {}", stats.venues);
    println!("children indexed  {}", stats.children);
    for (kind, venues) in &stats.by_kind {
        println!("  {kind:<20} {venues}");
    }
    for slot in manager.venues().iter().take(12) {
        let price = slot
            .price_state()
            .map_or_else(|| "—".to_owned(), |state| format!("tick {}", state.tick));
        println!("  {:<16} {} v{}  {}", slot.kind.label(), slot.id, slot.version, price);
    }
    if let Some(registry) = session.registry() {
        println!("\nlayout cache: {:?}", registry.stats());
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Offline replay
// ---------------------------------------------------------------------------

/// Capture everything the other commands need, so they can run with no node.
///
/// The capture records **raw Sui types** — `Object` BCS, resolved `MoveTypeLayout`s, and the parts
/// of the checkpoint — never derived answers. So `reproduce --fixtures …` walks exactly the same
/// decoders as `reproduce`, and its asserting the same `81_168_759` is a real check rather than a
/// replay of a stored number.
pub(crate) async fn fetch(session: &Session, out: &std::path::Path) -> eyre::Result<()> {
    if session.is_offline() {
        return Err(eyre::eyre!("`fetch` needs a node; it is what produces the fixture set"));
    }

    rule("capturing fixtures");
    let mut fixtures = Fixtures::default();
    fixtures.manifest.chain = session.objects.chain_id().await?;
    fixtures.manifest.rpc_url = session.rpc_url().unwrap_or_default().to_owned();
    fixtures.manifest.captured_at_unix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs());
    fixtures.manifest.note = String::from(
        "Checkpoints are filtered to the transactions that touch pool A, so the reassembled \
         checkpoint's object set is a subset of the chain's and tick-node counts reflect the \
         present dynamic-field index, not the checkpoint's version.",
    );

    // 1. The objects themselves.
    let targets: [(ObjectID, Option<u64>); 5] = [
        (object_id(POOL_A)?, None),
        (object_id(POOL_A)?, Some(POOL_A_PRE_VERSION)),
        (object_id(POOL_A)?, Some(POOL_A_POST_VERSION)),
        (object_id(POOL_B)?, None),
        (object_id(POOL_C)?, None),
    ];
    for (id, version) in targets {
        let decoded = session.decode(id, version).await?;
        println!("  object {} v{}", id, decoded.object.version().value());
        fixtures.record_object(&decoded.object)?;
    }

    // 2. The tick nodes of pool A, which are dynamic fields of an *inner* UID.
    let pool = load_pool(session, None).await?;
    let mut nodes_recorded = 0_usize;
    load_tick_nodes(session.objects.as_ref(), session.layouts.as_ref(), &pool.ticks, |object| {
        if fixtures.record_object(object).is_ok() {
            nodes_recorded += 1;
        }
    })
    .await?;
    println!("  tick nodes {nodes_recorded} (declared {})", pool.ticks.size);

    // 3. Checkpoint, filtered to the transactions that touch pool A.
    let checkpoint = session.checkpoint(TX_T_CHECKPOINT).await?;
    let watched = object_id(POOL_A)?;
    let mut kept = 0_usize;
    fixtures.record_checkpoint(&checkpoint, |executed| {
        let touches = sui_types::effects::TransactionEffectsAPI::object_changes(&executed.effects)
            .iter()
            .any(|change| change.id == watched);
        if touches {
            kept += 1;
        }
        touches
    })?;
    println!(
        "  checkpoint {TX_T_CHECKPOINT}: {kept} of {} transactions kept",
        checkpoint.transactions.len()
    );

    // 3b. Layouts for every venue-shaped object the checkpoint carries — including pools of
    // other deployments touched by the kept transactions, which the commands above never decoded
    // and the registry therefore never saw. Without this the capture would miss their packages
    // and an offline replay could not type them.
    let recorded = fixtures.checkpoint(TX_T_CHECKPOINT)?;
    let mut extra = 0_usize;
    for executed in &recorded.transactions {
        for change in sui_types::effects::TransactionEffectsAPI::object_changes(&executed.effects) {
            let Some(output_version) = change.output_version else { continue };
            let key = ObjectKey(change.id, output_version);
            let Some(object) = recorded.object_set.get(&key) else { continue };
            let Some(tag) = object.struct_tag() else { continue };
            if venue_kind_of(&tag).is_none() {
                continue;
            }
            // An object the node pruned between calls is not worth failing the capture for;
            // what matters is that everything still resolvable gets recorded.
            if session.layouts.layout(&tag).await.is_ok() {
                extra += 1;
            }
        }
    }
    println!("  checkpoint venue layouts pre-resolved {extra}");

    // 4. Layouts and the package bytecode they were resolved from.
    let mut addresses: std::collections::BTreeSet<AccountAddress> =
        std::collections::BTreeSet::new();
    let mut recorded = 0_usize;
    let Some(registry) = session.registry() else {
        return Err(eyre::eyre!("`fetch` needs an online registry to enumerate resolved layouts"));
    };
    for entry in registry.cached_layouts() {
        let (tag, layout) = entry;
        let canonical = session.layouts.canonical(&tag).await?;
        addresses.insert(canonical.address);
        for (address, _version) in
            birdai_resolve::layout::collect_dependencies(&layout, &std::collections::HashMap::new())
        {
            addresses.insert(address);
        }
        fixtures.record_layout(&tag, &canonical, &layout)?;
        recorded += 1;
    }
    // The entry the chain executed lives in a sibling package, which the layouts alone never name.
    addresses.insert(POOL_SCRIPT_PACKAGE.parse::<AccountAddress>()?);
    println!("  layouts {recorded}, packages {}", addresses.len());

    for address in &addresses {
        match session.objects.object(sui_types::base_types::ObjectID::from(*address), None).await {
            Ok(object) => fixtures.record_package(address, &object)?,
            Err(error) => println!("    package {address} unavailable: {error}"),
        }
    }

    fixtures.save(out)?;
    println!(
        "\nwrote {} ({} objects, {} packages, {} layouts, {} checkpoints)",
        out.display(),
        fixtures.objects.len(),
        fixtures.packages.len(),
        fixtures.layouts.len(),
        fixtures.checkpoints.len()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use birdai_venue::{NaviStorage, Venue, VenueKind, VoloNativePool, venue_kind_of};

    use super::{load_pool, move_in_ticks};
    use crate::{
        constants::{POOL_A_PRE_VERSION, POOL_B, POOL_C, object_id},
        session::Session,
    };

    #[test]
    fn the_transaction_moves_less_than_one_tick() {
        // Mainnet: S and S' for transaction T.
        let moved = move_in_ticks(647_308_812_393_509_050_120, 647_324_162_169_833_037_484);
        assert!(moved < 1.0, "moved {moved} ticks");
    }

    /// Objects B and C decode offline as a vault and a ledger: neither carries price state.
    ///
    /// Skipped when the fixture set is absent, like the committed fixture tests: the fixtures
    /// are required data, but a checkout without them should not look like a code failure.
    #[tokio::test]
    async fn vault_and_ledger_decode_offline_without_price_state() -> eyre::Result<()> {
        let dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures");
        if !dir.join("manifest.json").exists() {
            return Ok(());
        }
        let session = Session::offline(&dir)?;

        let decoded = session.decode(object_id(POOL_B)?, None).await?;
        assert_eq!(venue_kind_of(&decoded.tag), Some(VenueKind::VoloNativePool));
        let contents = decoded
            .object
            .data
            .try_as_move()
            .ok_or_else(|| eyre::eyre!("object B is not a Move object"))?
            .contents();
        let vault = VoloNativePool::decode(contents, &decoded.layout)?;
        assert!(vault.price_state().is_none(), "a vault quotes no price");

        let decoded = session.decode(object_id(POOL_C)?, None).await?;
        assert_eq!(venue_kind_of(&decoded.tag), Some(VenueKind::NaviStorage));
        let contents = decoded
            .object
            .data
            .try_as_move()
            .ok_or_else(|| eyre::eyre!("object C is not a Move object"))?
            .contents();
        let ledger = NaviStorage::decode(contents, &decoded.layout)?;
        assert!(ledger.price_state().is_none(), "a ledger quotes no price");

        // And the venue still does, at the version transaction T consumed.
        let pool = load_pool(&session, Some(POOL_A_PRE_VERSION)).await?;
        assert!(pool.price_state().is_some(), "the CLMM carries its price");
        Ok(())
    }
}
