//! This module builds a graph of relationships between symbols and linker sections. Provided code
//! was compiled with one symbol per section, which it should have been, there should be a 1:1
//! relationship between symbols and sections.
//!
//! We also parse the Dwarf debug information to determine what source file each linker section came
//! from.

use self::dwarf::SymbolDebugInfo;
use self::object_file_path::ObjectFilePath;
use crate::checker::ApiUsage;
use crate::checker::Checker;
use crate::config::CrateName;
use crate::config::PermissionName;
use crate::crate_index::CrateSel;
use crate::demangle::NonMangledIterator;
use crate::lazy::Lazy;
use crate::location::SourceLocation;
use crate::names::Name;
use crate::names::NamesIterator;
use crate::problem::ApiUsageGroupKey;
use crate::problem::ApiUsages;
use crate::problem::PossibleExportedApi;
use crate::problem::ProblemList;
use crate::symbol::Symbol;
use anyhow::anyhow;
use anyhow::bail;
use anyhow::Context;
use anyhow::Result;
use ar::Archive;
use fxhash::FxHashMap;
use fxhash::FxHashSet;
use gimli::Dwarf;
use gimli::EndianSlice;
use gimli::LittleEndian;
use log::debug;
use log::trace;
use object::Object;
use object::ObjectSection;
use object::ObjectSymbol;
use object::RelocationTarget;
use object::SectionIndex;
use std::borrow::Cow;
use std::collections::BTreeMap;
use std::collections::HashSet;
use std::fmt::Display;
use std::fs::File;
use std::io::Read;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

mod dwarf;
pub(crate) mod object_file_path;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Filetype {
    Archive,
    Other,
}

struct ApiUsageCollector<'input> {
    outputs: ScanOutputs,

    bin: BinInfo<'input>,
    debug_enabled: bool,
    new_api_usages: FxHashMap<ApiUsageGroupKey, Vec<ApiUsages>>,
}

/// Information derived from a linked binary. Generally an executable, but could also be shared
/// object (so).
struct BinInfo<'input> {
    filename: Arc<Path>,
    symbol_addresses: FxHashMap<Symbol<'input>, u64>,
    /// Symbols that we've already determined have no APIs. This is an optimisation that lets us
    /// skip these symbols when we see them again.
    symbol_has_no_apis: FxHashMap<Symbol<'input>, bool>,

    /// Information about each symbol obtained from the debug info.
    symbol_debug_info: FxHashMap<Symbol<'input>, SymbolDebugInfo<'input>>,
}

#[derive(Default)]
pub(crate) struct ScanOutputs {
    api_usages: Vec<ApiUsages>,

    /// Problems not related to api_usage. These can't be fixed by config changes via the UI, since
    /// once computed, they won't be recomputed.
    base_problems: ProblemList,

    possible_exported_apis: Vec<PossibleExportedApi>,
}

struct ObjectIndex<'obj, 'data> {
    obj: &'obj object::File<'data>,

    section_infos: Vec<SectionInfo<'data>>,
}

#[derive(Clone, Default)]
struct SectionInfo<'data> {
    first_symbol: Option<SymbolInfo<'data>>,
}

#[derive(Clone)]
struct SymbolInfo<'data> {
    /// The first symbol in the section.
    symbol: Symbol<'data>,

    /// The offset of the symbol.
    offset: u64,
}

pub(crate) fn scan_objects(
    paths: &[PathBuf],
    bin_path: &Path,
    checker: &mut Checker,
) -> Result<ScanOutputs> {
    log::info!("Scanning {}", bin_path.display());
    let start = Instant::now();
    let file_bytes = std::fs::read(bin_path)
        .with_context(|| format!("Failed to read `{}`", bin_path.display()))?;
    let obj = object::File::parse(file_bytes.as_slice())
        .with_context(|| format!("Failed to parse {}", bin_path.display()))?;
    let owned_dwarf = Dwarf::load(|id| load_section(&obj, id))?;
    let dwarf = owned_dwarf.borrow(|section| gimli::EndianSlice::new(section, gimli::LittleEndian));
    let start = checker.timings.add_timing(start, "Parse bin");
    let debug_artifacts = dwarf::DebugArtifacts::from_dwarf(&dwarf)?;
    let start = checker.timings.add_timing(start, "Read debug artifacts");
    let ctx = addr2line::Context::from_dwarf(dwarf)
        .with_context(|| format!("Failed to process {}", bin_path.display()))?;
    let start = checker.timings.add_timing(start, "Build addr2line context");

    let no_api_symbol_hashes = debug_artifacts
        .symbol_debug_info
        .keys()
        .map(|symbol| (symbol.clone(), false))
        .collect();
    let mut collector = ApiUsageCollector {
        outputs: Default::default(),
        bin: BinInfo {
            filename: Arc::from(bin_path),
            symbol_addresses: Default::default(),
            symbol_debug_info: debug_artifacts.symbol_debug_info,
            symbol_has_no_apis: no_api_symbol_hashes,
        },
        debug_enabled: checker.args.debug,
        new_api_usages: FxHashMap::default(),
    };
    collector.bin.load_symbols(&obj)?;
    let start = checker.timings.add_timing(start, "Load symbols from bin");
    for f in debug_artifacts.inlined_functions {
        let mut lazy_location = crate::lazy::lazy(|| f.location());
        collector.process_reference(
            &f.from_symbol,
            &f.to_symbol,
            checker,
            &mut lazy_location,
            None,
        )?;
    }
    let start = checker
        .timings
        .add_timing(start, "Process inlined references");
    collector.find_possible_exports(checker);
    let start = checker.timings.add_timing(start, "Find possible exports");
    for path in paths {
        collector
            .process_file(path, checker, &ctx)
            .with_context(|| format!("Failed to process `{}`", path.display()))?;
    }
    collector.emit_shortest_api_usages();
    checker.timings.add_timing(start, "Process object files");

    Ok(collector.outputs)
}

impl ScanOutputs {
    pub(crate) fn problems(&self, checker: &mut Checker) -> Result<ProblemList> {
        let mut problems: ProblemList = self.base_problems.clone();
        for api_usage in &self.api_usages {
            checker.permission_used(api_usage, &mut problems);
        }
        checker.possible_exported_api_problems(&self.possible_exported_apis, &mut problems);

        Ok(problems)
    }
}

impl<'input> ApiUsageCollector<'input> {
    fn process_file(
        &mut self,
        filename: &Path,
        checker: &Checker,
        ctx: &addr2line::Context<EndianSlice<'input, LittleEndian>>,
    ) -> Result<()> {
        let mut buffer = Vec::new();
        match Filetype::from_filename(filename) {
            Filetype::Archive => {
                let mut archive = Archive::new(File::open(filename)?);
                while let Some(entry_result) = archive.next_entry() {
                    let Ok(mut entry) = entry_result else {
                        continue;
                    };
                    buffer.clear();
                    entry.read_to_end(&mut buffer)?;
                    let object_file_path = ObjectFilePath::in_archive(filename, &entry)?;
                    self.process_object_file_bytes(&object_file_path, &buffer, checker, ctx)
                        .with_context(|| format!("Failed to process {object_file_path}"))?;
                }
            }
            Filetype::Other => {
                let file_bytes = std::fs::read(filename)
                    .with_context(|| format!("Failed to read `{}`", filename.display()))?;
                let object_file_path = ObjectFilePath::non_archive(filename);
                self.process_object_file_bytes(&object_file_path, &file_bytes, checker, ctx)
                    .with_context(|| format!("Failed to process {object_file_path}"))?;
            }
        }
        Ok(())
    }

    /// Processes an unlinked object file - as opposed to an executable or a shared object, which
    /// has been linked.
    fn process_object_file_bytes(
        &mut self,
        filename: &ObjectFilePath,
        file_bytes: &[u8],
        checker: &Checker,
        ctx: &addr2line::Context<EndianSlice<'input, LittleEndian>>,
    ) -> Result<()> {
        debug!("Processing object file {}", filename);

        let obj = object::File::parse(file_bytes).context("Failed to parse object file")?;
        let object_index = ObjectIndex::new(&obj);
        for section in obj.sections() {
            let section_name = section.name().unwrap_or("");
            let Some(first_sym_info) = object_index.first_symbol(&section) else {
                debug!("Skipping section `{section_name}` due to lack of debug info");
                continue;
            };
            let Some(symbol_address_in_bin) = self
                .bin
                .symbol_addresses
                .get(&first_sym_info.symbol)
                .cloned()
            else {
                debug!(
                    "Skipping section `{}` because symbol `{}` doesn't appear in exe/so",
                    section_name, first_sym_info.symbol
                );
                continue;
            };
            let Some(debug_info) = self.bin.symbol_debug_info.get(&first_sym_info.symbol) else {
                continue;
            };
            let fallback_source_location = debug_info.source_location();
            let debug_data = self.debug_enabled.then(|| UsageDebugData {
                bin_path: self.bin.filename.clone(),
                object_file_path: filename.clone(),
                section_name: section_name.to_owned(),
            });

            for (offset, rel) in section.relocations() {
                let mut target_symbols = Vec::new();
                let rel = &rel;
                object_index.add_target_symbols(rel, &mut target_symbols, &mut HashSet::new())?;
                let mut lazy_location = crate::lazy::lazy(|| {
                    Ok(
                        find_location(ctx, symbol_address_in_bin + offset - first_sym_info.offset)?
                            .unwrap_or_else(|| fallback_source_location.clone()),
                    )
                });
                for target_symbol in target_symbols {
                    let from_symbol = &first_sym_info.symbol;

                    self.process_reference(
                        from_symbol,
                        &target_symbol,
                        checker,
                        &mut lazy_location,
                        debug_data.as_ref(),
                    )?;
                }
            }
        }
        Ok(())
    }

    fn process_reference(
        &mut self,
        from_symbol: &Symbol,
        target_symbol: &Symbol,
        checker: &Checker,
        lazy_location: &mut impl Lazy<SourceLocation>,
        debug_data: Option<&UsageDebugData>,
    ) -> Result<(), anyhow::Error> {
        trace!("{from_symbol} -> {target_symbol}");

        let mut from_apis = HashSet::new();
        self.bin
            .names_and_apis_do(from_symbol, checker, |_, _, apis| {
                from_apis.extend(apis.iter());
                Ok(())
            })?;
        let mut lazy_crate_names = None;
        self.bin
            .names_and_apis_do(target_symbol, checker, |name, name_source, apis| {
                // For the majority of references we expect no APIs to match. We defer computation
                // of a source location and crate names until we know that an API matched.
                let location = lazy_location.get()?;
                if lazy_crate_names.is_none() {
                    lazy_crate_names =
                        Some(checker.crate_names_from_source_path(location.filename())?);
                }
                let crate_names = lazy_crate_names.as_ref().unwrap();

                for crate_sel in crate_names.as_ref() {
                    let crate_name = CrateName::from(crate_sel);
                    // If a package references another symbol within the same package,
                    // ignore it.
                    if name
                        .parts
                        .first()
                        .map(|name_start| crate_name.as_ref() == &**name_start)
                        .unwrap_or(false)
                    {
                        continue;
                    }
                    for permission in apis {
                        if from_apis.contains(&permission) {
                            continue;
                        }
                        let mut usages = BTreeMap::new();
                        usages.insert(
                            permission.clone(),
                            vec![ApiUsage {
                                source_location: location.clone(),
                                from: from_symbol.to_heap(),
                                to: name.clone(),
                                to_symbol: target_symbol.to_heap(),
                                to_source: name_source.to_owned(),
                                debug_data: debug_data.cloned(),
                            }],
                        );
                        let api_usage = ApiUsages {
                            crate_sel: crate_sel.clone(),
                            usages,
                        };
                        self.new_api_usages
                            .entry(api_usage.deduplication_key())
                            .or_default()
                            .push(api_usage);
                    }
                }
                Ok(())
            })?;
        Ok(())
    }

    fn emit_shortest_api_usages(&mut self) {
        // New API usages are grouped by their deduplication key, which doesn't include the target
        // symbol. We then output only the API usage with the shortest target symbol.
        for api_usages in std::mem::take(&mut self.new_api_usages).into_values() {
            if let Some(shortest_target_usage) = api_usages
                .into_iter()
                .min_by_key(|u| u.first_usage().unwrap().to_symbol.len())
            {
                self.outputs.api_usages.push(shortest_target_usage);
            }
        }
    }

    fn find_possible_exports(&mut self, checker: &Checker) {
        let api_names: FxHashMap<&str, &PermissionName> = checker
            .config
            .apis
            .keys()
            .map(|n| (n.name.as_ref(), n))
            .collect();
        let mut found = HashSet::new();
        for (symbol, debug_info) in &self.bin.symbol_debug_info {
            let Some(module_name) = symbol.module_name() else {
                continue;
            };
            let Some(permission_name) = api_names.get(module_name) else {
                continue;
            };
            let location = debug_info.source_location();
            for crate_sel in checker
                .opt_crate_names_from_source_path(location.filename())
                .unwrap_or_else(|| Cow::Owned(Vec::new()))
                .as_ref()
            {
                // APIs can only be exported from the primary crate in a package.
                let CrateSel::Primary(pkg_id) = crate_sel else {
                    continue;
                };
                if found.insert((pkg_id.clone(), permission_name)) {
                    // Macros can sometimes result in symbols being attributed to lower-level
                    // crates, so we only consider exported APIs that start with the crate name we
                    // expect for the package.
                    if symbol.crate_name() != Some(pkg_id.crate_name().as_ref()) {
                        continue;
                    }
                    self.outputs
                        .possible_exported_apis
                        .push(PossibleExportedApi {
                            pkg_id: pkg_id.to_owned(),
                            api: PermissionName::clone(permission_name),
                            symbol: symbol.to_heap(),
                        });
                }
            }
        }
    }
}

impl<'obj, 'data> ObjectIndex<'obj, 'data> {
    fn new(obj: &'obj object::File<'data>) -> Self {
        let max_section_index = obj.sections().map(|s| s.index().0).max().unwrap_or(0);
        let mut section_infos = vec![SectionInfo::default(); max_section_index + 1];
        for obj_symbol in obj.symbols() {
            let name = obj_symbol.name_bytes().unwrap_or_default();
            if name.is_empty() || !obj_symbol.is_definition() {
                continue;
            }
            let Some(section_index) = obj_symbol.section_index() else {
                continue;
            };
            let section_info = &mut section_infos[section_index.0];
            let symbol_is_first_in_section = section_info
                .first_symbol
                .as_ref()
                .map(|existing| obj_symbol.address() < existing.offset)
                .unwrap_or(true);
            if symbol_is_first_in_section {
                section_info.first_symbol = Some(SymbolInfo {
                    symbol: Symbol::borrowed(name),
                    offset: obj_symbol.address(),
                });
            }
        }
        Self { obj, section_infos }
    }

    /// Adds the symbol or symbols that `rel` refers to into `symbols_out`. If `rel` refers to a
    /// section that doesn't define a non-local symbol at address 0, then all outgoing references
    /// from that section will be included and so on recursively.
    fn add_target_symbols(
        &self,
        rel: &object::Relocation,
        symbols_out: &mut Vec<Symbol<'data>>,
        visited: &mut HashSet<SectionIndex>,
    ) -> Result<()> {
        match self.get_symbol_or_section(rel.target())? {
            SymbolOrSection::Symbol(symbol) => {
                symbols_out.push(symbol);
            }
            SymbolOrSection::Section(section_index) => {
                if !visited.insert(section_index) {
                    // We've already visited this section.
                    return Ok(());
                }
                let section = self.obj.section_by_index(section_index)?;
                for (_, rel) in section.relocations() {
                    self.add_target_symbols(&rel, symbols_out, visited)?;
                }
            }
        }
        Ok(())
    }

    /// Returns either symbol or the section index for a relocation target, giving preference to the
    /// symbol.
    fn get_symbol_or_section(&self, target_in: RelocationTarget) -> Result<SymbolOrSection<'data>> {
        let section_index = match target_in {
            RelocationTarget::Symbol(symbol_index) => {
                let Ok(symbol) = self.obj.symbol_by_index(symbol_index) else {
                    bail!("Invalid symbol index in object file");
                };
                let name = symbol.name_bytes().unwrap_or_default();
                if !name.is_empty() {
                    return Ok(SymbolOrSection::Symbol(Symbol::borrowed(name).to_heap()));
                }
                symbol.section_index().ok_or_else(|| {
                    anyhow!("Relocation target has empty name an no section index")
                })?
            }
            RelocationTarget::Section(_) => todo!(),
            _ => bail!("Unsupported relocation kind {target_in:?}"),
        };
        let section_info = &self
            .section_infos
            .get(section_index.0)
            .ok_or_else(|| anyhow!("Unnamed symbol has invalid section index"))?;
        if let Some(first_symbol_info) = section_info.first_symbol.as_ref() {
            return Ok(SymbolOrSection::Symbol(first_symbol_info.symbol.clone()));
        }
        Ok(SymbolOrSection::Section(section_index))
    }

    /// Returns information about the first symbol in the section.
    fn first_symbol(&self, section: &object::Section) -> Option<&SymbolInfo<'data>> {
        self.section_infos
            .get(section.index().0)
            .and_then(|section_info| section_info.first_symbol.as_ref())
    }
}

enum SymbolOrSection<'data> {
    Symbol(Symbol<'data>),
    Section(SectionIndex),
}

impl<'input> BinInfo<'input> {
    fn load_symbols(&mut self, obj: &object::File) -> Result<()> {
        for symbol in obj.symbols() {
            self.symbol_addresses.insert(
                Symbol::borrowed(symbol.name_bytes()?).to_heap(),
                symbol.address(),
            );
        }
        Ok(())
    }
}

fn find_location(
    ctx: &addr2line::Context<EndianSlice<LittleEndian>>,
    offset: u64,
) -> Result<Option<SourceLocation>> {
    use addr2line::Location;

    let Some(location) = ctx.find_location(offset).context("find_location failed")? else {
        return Ok(None);
    };
    let Location {
        file: Some(file),
        line: Some(line),
        column,
    } = location
    else {
        return Ok(None);
    };
    Ok(Some(SourceLocation::new(Path::new(file), line, column)))
}

impl<'input> BinInfo<'input> {
    /// Runs `callback` for each name in `symbol` or in the name obtained for the debug information
    /// for `symbol`. Also supplies information about the name source and a set of APIs that match
    /// the name.
    fn names_and_apis_do<'checker>(
        &mut self,
        symbol: &Symbol,
        checker: &'checker Checker,
        mut callback: impl FnMut(Name, NameSource, &'checker FxHashSet<PermissionName>) -> Result<()>,
    ) -> Result<()> {
        // If we've previously observed that this symbol has no APIs associated with it, then skip
        // it.
        if self
            .symbol_has_no_apis
            .get(symbol)
            .cloned()
            .unwrap_or(false)
        {
            return Ok(());
        }
        let mut got_apis = false;
        if let Some(target_symbol_debug) = self.symbol_debug_info.get(symbol) {
            if let Some(debug_name) = target_symbol_debug.name {
                let mut it = NamesIterator::new(NonMangledIterator::new(debug_name));
                let debug_name: Arc<str> = Arc::from(debug_name);
                while let Some((parts, name)) = it.next_name()? {
                    let apis = checker.apis_for_name_iterator(parts);
                    if !apis.is_empty() {
                        got_apis = true;
                        (callback)(
                            name.create_name()?,
                            NameSource::DebugName(debug_name.clone()),
                            apis,
                        )?;
                    }
                }
            }
        }
        let mut symbol_it = symbol.names()?;
        while let Some((parts, name)) = symbol_it.next_name()? {
            let apis = checker.apis_for_name_iterator(parts);
            if !apis.is_empty() {
                got_apis = true;
                (callback)(
                    name.create_name()?,
                    NameSource::Symbol(symbol.clone()),
                    apis,
                )?;
            }
        }
        if !got_apis {
            // The need to call `to_heap` here is just to get past an annoying variance issue.
            // Fortunately it doesn't seem to affect performance significantly, so probably the
            // optimiser is able to get rid of the allocation.
            if let Some(x) = self.symbol_has_no_apis.get_mut(&symbol.to_heap()) {
                *x = true;
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum NameSource<'symbol> {
    Symbol(Symbol<'symbol>),
    DebugName(Arc<str>),
}

impl<'symbol> NameSource<'symbol> {
    fn to_owned(&self) -> NameSource<'static> {
        match self {
            NameSource::Symbol(symbol) => NameSource::Symbol(symbol.to_heap()),
            NameSource::DebugName(name) => NameSource::DebugName(name.clone()),
        }
    }
}

impl<'symbol> Display for NameSource<'symbol> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            NameSource::Symbol(symbol) => symbol.fmt(f),
            NameSource::DebugName(debug_name) => debug_name.fmt(f),
        }
    }
}

/// Loads section `id` from `obj`.
fn load_section<'data>(
    obj: &object::File<'data>,
    id: gimli::SectionId,
) -> Result<Cow<'data, [u8]>, gimli::Error> {
    let Some(section) = obj.section_by_name(id.name()) else {
        return Ok(Cow::Borrowed([].as_slice()));
    };
    let Ok(data) = section.uncompressed_data() else {
        return Ok(Cow::Borrowed([].as_slice()));
    };
    Ok(data)
}

impl Filetype {
    fn from_filename(filename: &Path) -> Self {
        let Some(extension) = filename.extension() else {
            return Filetype::Other;
        };
        if extension == "rlib" || extension == ".a" {
            Filetype::Archive
        } else {
            Filetype::Other
        }
    }
}

/// Additional information that might be useful for debugging. Only available when --debug is
/// passed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct UsageDebugData {
    bin_path: Arc<Path>,
    object_file_path: ObjectFilePath,
    section_name: String,
}
