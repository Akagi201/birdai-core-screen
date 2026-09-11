//! The on-chain price-discovery test.
//!
//! # The test
//!
//! An object `O` of type `T` is a trading venue with on-chain price discovery **iff** all three
//! probes pass.
//!
//! 1. **Inter-asset swap entry** *(static, from package bytecode)*. The module defining `T` must
//!    expose a public (or entry) function that takes `&mut T` together with a
//!    `Coin<X>`/`Balance<X>` and returns a `Coin<Y>`/`Balance<Y>`, where `X` and `Y` are **two
//!    different type parameters of `T`**. Nothing about field names or about "having balances"
//!    enters here: the probe reads [`FunctionDef`] signatures, so a vault with two `Balance` fields
//!    and no inter-asset swap fails it immediately.
//! 2. **Endogenous price state** *(empirical, from two versions)*. `O` must carry a price variable
//!    that is a pure function of its own fields, and that variable must **move with net flow**:
//!    across a transaction that used the entry found in (1), it moved in the direction the flow
//!    implies, and no oracle object was among that transaction's inputs.
//! 3. **No oracle dependency** *(static)*. Neither `T`'s field types nor the defining module's
//!    dependencies may reference an external price feed.
//!
//! Probe (1) is what distinguishes a venue from a vault; probe (2) is what distinguishes "has a
//! price field" from "discovers a price"; probe (3) is what distinguishes price discovery from
//! price import.
//!
//! # Why this is the right cut
//!
//! A liquid-staking pool and a lending ledger both hold balances and both let you move value, but
//! neither *quotes* a price out of its own state. Volo's SUI↔VSUI rate is an accounting ratio that
//! only moves when staking rewards accrue; Navi's asset values come from an oracle and its interest
//! rates from a utilisation curve. Only the CLMM's `sqrt_price` is a price that trading moves.

use birdai_move::tag::{balance_inner, coin_inner, short_tag};
use birdai_resolve::LayoutSource;
use move_binary_format::file_format::Visibility;
use move_core_types::{
    account_address::AccountAddress, annotated_value::MoveTypeLayout, language_storage::StructTag,
};
use sui_package_resolver::{FunctionDef, Module, OpenSignature, OpenSignatureBody, Reference};
use sui_types::object::Object;

use crate::{cetus::CetusClmm, error::VenueError, venue::PriceState};

/// Module-name fragments that mean "this package imports prices from outside".
///
/// This is a *dependency* heuristic, not a field-name one: it is applied to the modules a package
/// links against, and a venue that discovered its own prices would not link an oracle at all.
const ORACLE_NAME_FRAGMENTS: &[&str] =
    &["oracle", "pyth", "supra", "switchboard", "price_feed", "price_info"];

/// How many coin-touching signatures to carry in the evidence.
///
/// Enough to audit the verdict by eye, few enough that a package with dozens of them does not
/// flood the report.
const MAX_REPORTED_COIN_FUNCTIONS: usize = 12;

/// Which packages and module names count as oracles.
#[derive(Debug, Clone)]
pub struct OracleDenySet {
    addresses: Vec<AccountAddress>,
    fragments: Vec<String>,
}

impl Default for OracleDenySet {
    fn default() -> Self {
        Self::default_mainnet()
    }
}

impl OracleDenySet {
    /// The default deny list: name fragments plus any explicitly configured addresses.
    #[must_use]
    pub fn default_mainnet() -> Self {
        Self {
            addresses: Vec::new(),
            fragments: ORACLE_NAME_FRAGMENTS
                .iter()
                .map(|fragment| (*fragment).to_owned())
                .collect(),
        }
    }

    /// Add a package address that should always be treated as an oracle.
    #[must_use]
    pub fn with_address(mut self, address: AccountAddress) -> Self {
        self.addresses.push(address);
        self
    }

    /// True when a module/package pair looks like an oracle.
    #[must_use]
    pub fn matches(&self, package: &AccountAddress, module: &str) -> bool {
        if self.addresses.contains(package) {
            return true;
        }
        let module = module.to_ascii_lowercase();
        self.fragments.iter().any(|fragment| module.contains(fragment.as_str()))
    }
}

/// How an entry was found, which determines how strictly it had to match.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryEvidence {
    /// A scan of the defining package's bytecode for the shape.
    ///
    /// Scans must be strict, because a package contains many functions that borrow the venue and
    /// touch its assets without being an exchange — `remove_liquidity` and `collect_fee` both
    /// return both of a pool's assets and neither discovers a price.
    StaticScan,
    /// A function the chain actually executed, resolved by its `package::module::function`.
    ///
    /// Direct evidence, so a looser shape test is warranted: script-style entries such as Cetus's
    /// `pool_script_v2::swap_b2a` take both coins and write the result into one of them, so the
    /// input/output split lives in a `bool` direction flag rather than in the types.
    ObservedCall,
}

/// The function that lets two assets be exchanged, as read from bytecode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SwapEntry {
    /// Module name.
    pub module: String,
    /// Function name. Recorded for evidence only — the probe never matches on it.
    pub function: String,
    /// Whether the function is marked `entry`.
    pub is_entry: bool,
    /// The function's type parameters that carry an asset leg, sorted.
    ///
    /// Two or more distinct indices is the structural statement of "this moves one of the venue's
    /// assets into another".
    pub asset_type_parameters: Vec<u16>,
    /// Rendered parameter list, for the report.
    pub parameters: Vec<String>,
    /// Rendered return list, for the report.
    pub returns: Vec<String>,
    /// How this entry was found.
    pub evidence: EntryEvidence,
}

/// Where an oracle dependency was found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OracleReference {
    /// Package address.
    pub package: String,
    /// Module name.
    pub module: String,
    /// Where it was seen.
    pub via: OracleVia,
}

/// How an oracle reference was discovered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OracleVia {
    /// The defining module links the oracle module.
    ModuleDependency,
    /// The object's type mentions an oracle type.
    FieldType,
    /// The swap entry's signature mentions an oracle type.
    SwapSignature,
}

/// Everything the static probes can say about a type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StaticEvidence {
    /// The swap entry, if one was found.
    pub swap_entry: Option<SwapEntry>,
    /// Oracle references, if any.
    pub oracle_references: Vec<OracleReference>,
    /// Number of functions examined, for context in the report.
    pub functions_examined: usize,
    /// Callable functions whose signature mentions a `Coin` or `Balance`.
    ///
    /// Printed when the swap probe fails, so the conclusion is checkable rather than asserted: it
    /// shows exactly which signatures were considered and why none of them exchanges two of the
    /// object's own assets.
    pub coin_functions: Vec<String>,
}

/// How the price state moved across a transaction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PriceStateChange {
    /// Price state before.
    pub before: PriceState,
    /// Price state after.
    pub after: PriceState,
    /// Change in the token A reserve, signed.
    pub coin_a_delta: i128,
    /// Change in the token B reserve, signed.
    pub coin_b_delta: i128,
    /// Oracle-looking objects among the transaction's inputs.
    pub oracle_inputs: Vec<String>,
}

impl PriceStateChange {
    /// True when the price moved *because* the object was traded, with no oracle involved.
    ///
    /// Selling B must raise `√P` and selling A must lower it; a price that moved the other way, or
    /// did not move at all, means the change did not come from the trade.
    #[must_use]
    pub const fn is_endogenous(&self) -> bool {
        if !self.oracle_inputs.is_empty() {
            return false;
        }
        let b_in = self.coin_b_delta > 0;
        let a_in = self.coin_a_delta > 0;
        match (a_in, b_in) {
            (false, true) => self.after.sqrt_price > self.before.sqrt_price,
            (true, false) => self.after.sqrt_price < self.before.sqrt_price,
            _ => false,
        }
    }

    /// Human-readable explanation of the verdict on this probe.
    #[must_use]
    pub fn explain(&self) -> String {
        if !self.oracle_inputs.is_empty() {
            return format!("oracle objects among the inputs: {}", self.oracle_inputs.join(", "));
        }
        let direction = match (self.coin_a_delta > 0, self.coin_b_delta > 0) {
            (false, true) => "B in, so sqrt price must rise",
            (true, false) => "A in, so sqrt price must fall",
            _ => "no net flow in either reserve",
        };
        format!(
            "{direction}; sqrt price {} -> {} (liquidity {} -> {}, tick {} -> {})",
            self.before.sqrt_price,
            self.after.sqrt_price,
            self.before.liquidity,
            self.after.liquidity,
            self.before.tick,
            self.after.tick
        )
    }
}

/// One probe's outcome.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Probe {
    /// Short probe name.
    pub name: &'static str,
    /// Whether the object passed.
    pub passed: bool,
    /// What the probe saw, in one line.
    pub detail: String,
}

/// The classifier's conclusion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Verdict {
    /// The type that was classified, rendered.
    pub tag: String,
    /// Every probe, in the order they were applied.
    pub probes: Vec<Probe>,
}

impl Verdict {
    /// True when every probe passed.
    #[must_use]
    pub fn is_price_discovery_venue(&self) -> bool {
        self.probes.iter().all(|probe| probe.passed)
    }

    /// Render the verdict as the paragraph the README needs.
    #[must_use]
    pub fn render(&self) -> String {
        let mut out = format!(
            "{} → {}\n",
            self.tag,
            if self.is_price_discovery_venue() {
                "TRADING VENUE with on-chain price discovery"
            } else {
                "not a trading venue"
            }
        );
        for probe in &self.probes {
            out.push_str(&format!(
                "  [{}] {}: {}\n",
                if probe.passed { "pass" } else { "FAIL" },
                probe.name,
                probe.detail
            ));
        }
        out
    }
}

/// Applies the three probes.
#[derive(Debug, Clone)]
pub struct Classifier {
    deny: OracleDenySet,
}

impl Default for Classifier {
    fn default() -> Self {
        Self::new(OracleDenySet::default_mainnet())
    }
}

impl Classifier {
    /// Build a classifier with an explicit oracle deny list.
    #[must_use]
    pub const fn new(deny: OracleDenySet) -> Self {
        Self { deny }
    }

    /// The deny list in use.
    #[must_use]
    pub const fn deny_set(&self) -> &OracleDenySet {
        &self.deny
    }

    /// Run the two static probes against `tag`.
    ///
    /// Requires the defining package's bytecode, which comes from the layout source.
    pub async fn static_evidence<L: LayoutSource + ?Sized>(
        &self,
        layouts: &L,
        tag: &StructTag,
    ) -> Result<StaticEvidence, VenueError> {
        // The layout is resolved first so a bad type fails before any package is fetched.
        let layout = layouts.layout(tag).await?;
        let package = layouts.package(tag.address).await?;

        // Both probes scan the **whole defining package**, not just the module that defines the
        // type. That is not a convenience: Cetus's `pool` module has no inter-asset swap at all —
        // its swaps are reached through a sibling router module — and a lending ledger's pricing
        // logic likewise lives in a sibling of its storage module.
        let mut swap_entry = None;
        let mut coin_functions = Vec::new();
        let mut module_count = 0_usize;
        let mut function_count = 0_usize;
        for (name, sibling) in package.modules() {
            module_count += 1;
            function_count += sibling.functions(None, None).count();
            if swap_entry.is_none() {
                swap_entry = find_inter_asset_swap(name, sibling).map_err(|message| {
                    VenueError::Function {
                        module: name.clone(),
                        function: "<scan>".to_owned(),
                        message,
                    }
                })?;
            }
            if coin_functions.len() < MAX_REPORTED_COIN_FUNCTIONS {
                coin_functions.extend(coin_signatures(name, sibling));
            }
        }

        let mut oracle_references = Vec::new();
        for (name, sibling) in package.modules() {
            oracle_references.extend(scan_module_dependencies(name, sibling, &self.deny));
        }
        scan_layout_for_oracles(&layout, &self.deny, &mut oracle_references);
        if let Some(entry) = &swap_entry {
            for rendered in entry.parameters.iter().chain(entry.returns.iter()) {
                if let Some(reference) = reference_from_text(rendered, &self.deny) {
                    oracle_references.push(reference);
                }
            }
        }
        oracle_references.sort_by(|a, b| a.module.cmp(&b.module));
        oracle_references.dedup();

        tracing::debug!(
            modules = module_count,
            functions = function_count,
            swap = swap_entry.as_ref().map(|entry| format!("{}::{}", entry.module, entry.function)),
            "scanned the defining package"
        );

        Ok(StaticEvidence {
            swap_entry,
            oracle_references,
            functions_examined: function_count,
            coin_functions,
        })
    }

    /// Resolve a function the chain actually called and test whether it has the swap shape.
    ///
    /// The defining package is not always where the entry lives: a mainnet Cetus swap of pool A
    /// went through `pool_script_v2::swap_b2a`, a sibling package that borrows the pool and moves
    /// its two assets. Testing the signature of the function the chain *did* call is stronger
    /// evidence than scanning for a shape somebody might have written.
    pub async fn entry_at<L: LayoutSource + ?Sized>(
        &self,
        layouts: &L,
        package: AccountAddress,
        module: &str,
        function: &str,
    ) -> Result<Option<SwapEntry>, VenueError> {
        let package = layouts.package(package).await?;
        let module = package.module(module).map_err(|error| VenueError::Module {
            package: package.storage_id().to_canonical_string(true),
            module: format!("{module} ({error})"),
        })?;
        let definition = module
            .function_def(function)
            .map_err(|error| VenueError::Function {
                module: function.to_owned(),
                function: function.to_owned(),
                message: error.to_string(),
            })?
            .ok_or_else(|| VenueError::Function {
                module: function.to_owned(),
                function: function.to_owned(),
                message: "the function is not in the module the transaction named".to_owned(),
            })?;
        if !is_callable(&definition) {
            return Ok(None);
        }
        Ok(swap_shape(module.name(), function, &definition, EntryEvidence::ObservedCall))
    }

    /// Turn static evidence, plus an optional observed price move, into a verdict.
    ///
    /// `observed` is the entry the chain actually executed, when a transaction is available. It
    /// takes precedence over the static scan because it is direct evidence rather than a search.
    #[must_use]
    pub fn verdict(
        &self,
        tag: &StructTag,
        evidence: &StaticEvidence,
        change: Option<&PriceStateChange>,
        observed: Option<&SwapEntry>,
    ) -> Verdict {
        let mut probes = Vec::with_capacity(3);

        let (passed, detail) = if let Some(entry) = observed.or(evidence.swap_entry.as_ref()) {
            (
                true,
                format!(
                    "`{}::{}` ({:?}) mutably borrows the object's generic state and carries asset \
                 legs on type parameters {:?}: ({}) -> ({})",
                    entry.module,
                    entry.function,
                    entry.evidence,
                    entry.asset_type_parameters,
                    entry.parameters.join(", "),
                    entry.returns.join(", ")
                ),
            )
        } else {
            let mut detail = format!(
                "none of the {} functions in `{}` exchanges one of the type's own coin \
                 parameters for another with a `&mut` borrow of the object",
                evidence.functions_examined, tag.module
            );
            if evidence.coin_functions.is_empty() {
                detail.push_str("; no callable function mentions a Coin or a Balance at all");
            } else {
                detail.push_str("; the coin-touching signatures considered were: ");
                detail.push_str(&evidence.coin_functions.join(" | "));
            }
            (false, detail)
        };
        probes.push(Probe { name: "inter-asset swap entry", passed, detail });

        let (passed, detail) = match change {
            Some(change) => (change.is_endogenous(), change.explain()),
            None => (
                false,
                "no transaction observed for this object, so the price state could not be \
                 checked against flow"
                    .to_owned(),
            ),
        };
        probes.push(Probe { name: "endogenous price state", passed, detail });

        let (passed, detail) = if evidence.oracle_references.is_empty() {
            (true, "no oracle dependency found".to_owned())
        } else {
            (
                false,
                evidence
                    .oracle_references
                    .iter()
                    .map(|reference| {
                        format!(
                            "{}/{} via {:?}",
                            reference.package, reference.module, reference.via
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("; "),
            )
        };
        probes.push(Probe { name: "no oracle dependency", passed, detail });

        Verdict { tag: short_tag(tag), probes }
    }

    /// Classify an object end to end, including the behavioural probe when a transaction is given.
    pub async fn classify_object<L: LayoutSource + ?Sized>(
        &self,
        layouts: &L,
        object: &Object,
        change: Option<&PriceStateChange>,
        observed: Option<&SwapEntry>,
    ) -> Result<Verdict, VenueError> {
        let tag = object
            .struct_tag()
            .ok_or_else(|| VenueError::NoTypeTag(object.id().to_canonical_string(true)))?;
        let evidence = self.static_evidence(layouts, &tag).await?;
        Ok(self.verdict(&tag, &evidence, change, observed))
    }
}

/// Probe 1: find a public/entry function that swaps one of the object's own coin parameters for
/// another, with a mutable borrow of the object itself.
///
/// Matching is entirely on `FunctionDef` structure — `OpenSignatureBody::Datatype` for the coin
/// types and `OpenSignatureBody::TypeParameter` for the flows — so it cannot be satisfied by
/// naming, and it cannot be satisfied by a vault that moves one asset.
pub fn find_inter_asset_swap(
    module_name: &str,
    module: &Module,
) -> Result<Option<SwapEntry>, String> {
    for name in module.functions(None, None) {
        let definition = module.function_def(name).map_err(|error| error.to_string())?;
        let Some(definition) = definition else {
            continue;
        };
        if !is_callable(&definition) {
            continue;
        }
        if let Some(entry) = swap_shape(module_name, name, &definition, EntryEvidence::StaticScan) {
            return Ok(Some(entry));
        }
    }
    Ok(None)
}

fn is_callable(definition: &FunctionDef) -> bool {
    definition.is_entry || definition.visibility == Visibility::Public
}

fn swap_shape(
    module_name: &str,
    name: &str,
    definition: &FunctionDef,
    evidence: EntryEvidence,
) -> Option<SwapEntry> {
    // The venue's own state must be mutated, and it must be a *generic* object: the assets being
    // exchanged are its type parameters, so a function that mutably borrows something with none —
    // a registry, a factory, a capability, a lending ledger — cannot be trading the object's assets
    // for each other. This one clause rejects Volo's `stake`/`unstake` and every Navi entry,
    // because neither `NativePool` nor `Storage` has a type parameter.
    let mutates_generic_state = definition
        .parameters
        .iter()
        .any(|parameter| is_mutable_generic_reference(&parameter.body, parameter.ref_));
    if !mutates_generic_state {
        return None;
    }

    // Every type parameter that carries an asset leg, on either side.
    let mut asset_type_parameters: Vec<u16> = definition
        .parameters
        .iter()
        .filter_map(|parameter| asset_type_parameter(&parameter.body))
        .chain(
            definition.return_.iter().filter_map(|returned| asset_type_parameter(&returned.body)),
        )
        .collect();
    asset_type_parameters.sort_unstable();
    asset_type_parameters.dedup();
    if asset_type_parameters.len() < 2 {
        return None;
    }

    if evidence == EntryEvidence::StaticScan {
        // A scan has to be strict, because a package is full of functions that borrow the venue and
        // hand back both of its assets without discovering a price — `remove_liquidity` and
        // `collect_fee` are the obvious ones. Requiring a coin to be *taken in* on one type
        // parameter and a different one to come *out* is what makes it an exchange.
        let inputs: Vec<u16> = definition
            .parameters
            .iter()
            .filter_map(|parameter| coin_input_type_parameter(&parameter.body))
            .collect();
        let outputs: Vec<u16> = definition
            .return_
            .iter()
            .filter_map(|returned| asset_type_parameter(&returned.body))
            .collect();
        if !inputs.iter().any(|input| outputs.iter().any(|output| output != input)) {
            return None;
        }
    }

    Some(SwapEntry {
        module: module_name.to_owned(),
        function: name.to_owned(),
        is_entry: definition.is_entry,
        asset_type_parameters,
        parameters: definition.parameters.iter().map(render_signature).collect(),
        returns: definition.return_.iter().map(render_signature).collect(),
        evidence,
    })
}

/// True when this is a `&mut D<…, TypeParameter, …>` — a mutable borrow of a generic struct.
fn is_mutable_generic_reference(body: &OpenSignatureBody, reference: Option<Reference>) -> bool {
    if !matches!(reference, Some(Reference::Mutable)) {
        return false;
    }
    match body {
        OpenSignatureBody::Datatype(_, arguments) => {
            arguments.iter().any(|argument| matches!(argument, OpenSignatureBody::TypeParameter(_)))
        }
        _ => false,
    }
}

/// The type parameter inside `Coin<T>` or `Balance<T>`, in any position or reference mode.
fn asset_type_parameter(body: &OpenSignatureBody) -> Option<u16> {
    coin_type_parameter(body, true)
}

/// The type parameter inside a *coin handed in*: `Coin<T>`, never `Balance<T>`.
///
/// A `Balance<T>` parameter is a caller-supplied deposit rather than an exact-input leg.
fn coin_input_type_parameter(body: &OpenSignatureBody) -> Option<u16> {
    coin_type_parameter(body, false)
}

/// The type parameter inside `Coin<T>` or `Balance<T>`.
///
/// `allow_balance` distinguishes the two directions an exchange can be expressed in: a `Coin` is
/// always a leg, while a `Balance` is one only on the return side.
fn coin_type_parameter(body: &OpenSignatureBody, allow_balance: bool) -> Option<u16> {
    let OpenSignatureBody::Datatype(key, arguments) = body else {
        return None;
    };
    let is_coin = key.module == "coin" && key.name == "Coin";
    let is_balance = key.module == "balance" && key.name == "Balance";
    if !(is_coin || (allow_balance && is_balance)) {
        return None;
    }
    match arguments.first()? {
        OpenSignatureBody::TypeParameter(index) => Some(*index),
        _ => None,
    }
}

fn render_signature(signature: &OpenSignature) -> String {
    let prefix = match signature.ref_ {
        Some(Reference::Mutable) => "&mut ",
        Some(Reference::Immutable) => "&",
        None => "",
    };
    format!("{prefix}{}", render_body(&signature.body))
}

fn render_body(body: &OpenSignatureBody) -> String {
    match body {
        OpenSignatureBody::Address => "address".to_owned(),
        OpenSignatureBody::Bool => "bool".to_owned(),
        OpenSignatureBody::U8 => "u8".to_owned(),
        OpenSignatureBody::U16 => "u16".to_owned(),
        OpenSignatureBody::U32 => "u32".to_owned(),
        OpenSignatureBody::U64 => "u64".to_owned(),
        OpenSignatureBody::U128 => "u128".to_owned(),
        OpenSignatureBody::U256 => "u256".to_owned(),
        OpenSignatureBody::Vector(inner) => format!("vector<{}>", render_body(inner)),
        OpenSignatureBody::Datatype(key, arguments) => {
            let mut rendered = format!("{}::{}", key.module, key.name);
            if !arguments.is_empty() {
                rendered.push('<');
                rendered
                    .push_str(&arguments.iter().map(render_body).collect::<Vec<_>>().join(", "));
                rendered.push('>');
            }
            rendered
        }
        OpenSignatureBody::TypeParameter(index) => format!("T{index}"),
    }
}

/// Probe 3a: modules that `module` links against, tagged with which module did the linking.
pub fn scan_module_dependencies(
    linking_module: &str,
    module: &Module,
    deny: &OracleDenySet,
) -> Vec<OracleReference> {
    let bytecode = module.bytecode();
    let mut found = Vec::new();
    for handle in bytecode.module_handles() {
        let name = bytecode.identifier_at(handle.name).as_str();
        let package = *bytecode.address_identifier_at(handle.address);
        if deny.matches(&package, name) {
            found.push(OracleReference {
                package: package.to_canonical_string(true),
                module: format!("{linking_module} → {name}"),
                via: OracleVia::ModuleDependency,
            });
        }
    }
    found
}

/// Callable functions in `module` whose signature mentions a `Coin` or a `Balance`.
///
/// Rendered for the report so a failed swap probe can be audited line by line.
#[must_use]
pub fn coin_signatures(module_name: &str, module: &Module) -> Vec<String> {
    let mut out = Vec::new();
    for name in module.functions(None, None) {
        let Ok(Some(definition)) = module.function_def(name) else {
            continue;
        };
        if !is_callable(&definition) {
            continue;
        }
        let parameters: Vec<String> = definition.parameters.iter().map(render_signature).collect();
        let returns: Vec<String> = definition.return_.iter().map(render_signature).collect();
        let mentions_coin = parameters
            .iter()
            .chain(returns.iter())
            .any(|text| text.contains("::Coin<") || text.contains("::Balance<"));
        if mentions_coin {
            out.push(format!(
                "{module_name}::{name}({}) -> ({})",
                parameters.join(", "),
                returns.join(", ")
            ));
        }
    }
    out
}

/// Probe 3b: oracle-looking types anywhere in the object's layout.
pub fn scan_layout_for_oracles(
    layout: &MoveTypeLayout,
    deny: &OracleDenySet,
    out: &mut Vec<OracleReference>,
) {
    match layout {
        MoveTypeLayout::Vector(inner) => scan_layout_for_oracles(inner, deny, out),
        MoveTypeLayout::Struct(inner) => {
            record_if_oracle(&inner.type_, deny, out);
            for field in &inner.fields {
                scan_layout_for_oracles(&field.layout, deny, out);
            }
        }
        MoveTypeLayout::Enum(inner) => {
            record_if_oracle(&inner.type_, deny, out);
            for fields in inner.variants.values() {
                for field in fields {
                    scan_layout_for_oracles(&field.layout, deny, out);
                }
            }
        }
        _ => {}
    }
}

fn record_if_oracle(tag: &StructTag, deny: &OracleDenySet, out: &mut Vec<OracleReference>) {
    if deny.matches(&tag.address, tag.module.as_str()) {
        out.push(OracleReference {
            package: tag.address.to_canonical_string(true),
            module: tag.module.to_string(),
            via: OracleVia::FieldType,
        });
    }
}

fn reference_from_text(text: &str, deny: &OracleDenySet) -> Option<OracleReference> {
    let lowered = text.to_ascii_lowercase();
    deny.fragments.iter().find(|fragment| lowered.contains(fragment.as_str())).map(|fragment| {
        OracleReference {
            package: "<in signature>".to_owned(),
            module: fragment.clone(),
            via: OracleVia::SwapSignature,
        }
    })
}

/// Observe a transaction's effect on a Cetus pool, for probe 2.
///
/// Takes the transaction's input objects as a slice rather than a checkpoint type, so the probe
/// works whether the objects came from a checkpoint's deduplicated object set or from a validator's
/// in-memory input set.
#[must_use]
pub fn price_state_change(
    before: &CetusClmm,
    after: &CetusClmm,
    inputs: &[&Object],
    deny: &OracleDenySet,
) -> PriceStateChange {
    let oracle_inputs = inputs
        .iter()
        .filter_map(|object| {
            let tag = object.struct_tag()?;
            deny.matches(&tag.address, tag.module.as_str()).then(|| short_tag(&tag))
        })
        .collect();

    PriceStateChange {
        before: PriceState {
            sqrt_price: before.sqrt_price,
            liquidity: before.liquidity,
            tick: before.tick,
        },
        after: PriceState {
            sqrt_price: after.sqrt_price,
            liquidity: after.liquidity,
            tick: after.tick,
        },
        coin_a_delta: i128::from(after.coin_a) - i128::from(before.coin_a),
        coin_b_delta: i128::from(after.coin_b) - i128::from(before.coin_b),
        oracle_inputs,
    }
}

/// Rendered types referenced by a layout, for reports.
#[must_use]
pub fn referenced_types(layout: &MoveTypeLayout) -> Vec<String> {
    let mut out = Vec::new();
    collect_types(layout, &mut out);
    out
}

fn collect_types(layout: &MoveTypeLayout, out: &mut Vec<String>) {
    match layout {
        MoveTypeLayout::Vector(inner) => collect_types(inner, out),
        MoveTypeLayout::Struct(inner) => {
            out.push(short_tag(&inner.type_));
            for field in &inner.fields {
                collect_types(&field.layout, out);
            }
        }
        MoveTypeLayout::Enum(inner) => {
            out.push(short_tag(&inner.type_));
        }
        other => out.push(birdai_move::dump::short_type_of(other)),
    }
}

/// True when a tag is `Coin<T>` or `Balance<T>`, with `T` returned.
#[must_use]
pub fn held_asset(tag: &StructTag) -> Option<&StructTag> {
    let inner = coin_inner(tag).or_else(|| balance_inner(tag))?;
    match inner {
        move_core_types::language_storage::TypeTag::Struct(tag) => Some(tag),
        _ => None,
    }
}
