#[cfg(feature = "cpp_demangle")]
extern crate cpp_demangle;
extern crate fallible_iterator;
extern crate gimli;
extern crate intervaltree;
extern crate lazy_init;
extern crate object;
#[cfg(feature = "rustc-demangle")]
extern crate rustc_demangle;
extern crate smallvec;

use std::path::PathBuf;
use std::cmp::Ordering;
use std::borrow::Cow;
use std::u64;

use fallible_iterator::FallibleIterator;
use intervaltree::{Element, IntervalTree};
use lazy_init::Lazy;
use object::Object;
use smallvec::SmallVec;

struct Func<T> {
    entry_off: gimli::UnitOffset<T>,
    depth: isize,
}

struct Lines<R: gimli::Reader> {
    lnp: gimli::CompleteLineNumberProgram<R>,
    sequences: Vec<gimli::LineNumberSequence<R>>,
}

struct ResUnit<R>
where
    R: gimli::Reader + Sync,
    R::Offset: Sync,
{
    dw_unit: gimli::CompilationUnitHeader<R, R::Offset>,
    abbrevs: gimli::Abbreviations,
    line_offset: gimli::DebugLineOffset<R::Offset>,
    comp_dir: Option<R>,
    comp_name: Option<R>,
    lang: Option<gimli::DwLang>,
    base_addr: u64,
    lines: Lazy<Result<Lines<R>, Error>>,
    funcs: Lazy<Result<IntervalTree<u64, Func<R::Offset>>, Error>>,
}

pub struct Context<R>
where
    R: gimli::Reader + Sync,
    R::Offset: Sync,
{
    unit_ranges: Vec<(gimli::Range, usize)>,
    units: Vec<ResUnit<R>>,
    sections: DebugSections<R>,
}

fn read_ranges<R: gimli::Reader>(
    entry: &gimli::DebuggingInformationEntry<R, R::Offset>,
    debug_ranges: &gimli::DebugRanges<R>,
    addr_size: u8,
    base_addr: u64,
) -> Result<Option<WrapRangeIter<R>>, Error> {
    Ok(Some(match entry.attr_value(gimli::DW_AT_ranges)? {
        None => {
            let low_pc = match entry.attr_value(gimli::DW_AT_low_pc)? {
                Some(gimli::AttributeValue::Addr(low_pc)) => low_pc,
                _ => return Ok(None), // neither ranges nor low_pc => None
            };
            let high_pc = match entry.attr_value(gimli::DW_AT_high_pc)? {
                Some(gimli::AttributeValue::Addr(high_pc)) => high_pc,
                Some(gimli::AttributeValue::Udata(x)) => low_pc + x,
                _ => return Ok(None), // only low_pc, no high_pc? wtf is this? TODO: perhaps return error
            };
            WrapRangeIter::Synthetic(Some(gimli::Range {
                begin: low_pc,
                end: high_pc,
            }))
        }
        Some(gimli::AttributeValue::DebugRangesRef(rr)) => {
            let ranges = debug_ranges.ranges(rr, addr_size, base_addr)?;
            WrapRangeIter::Real(ranges)
        }
        _ => unreachable!(),
    }))
}

impl<'a> Context<gimli::EndianBuf<'a, gimli::RunTimeEndian>> {
    pub fn new(file: &'a object::File) -> Result<Self, Error> {
        let endian = if file.is_little_endian() {
            gimli::RunTimeEndian::Little
        } else {
            gimli::RunTimeEndian::Big
        };

        fn load_section<'input, 'file, S, Endian>(
            file: &'file object::File<'input>,
            endian: Endian,
        ) -> S
        where
            S: gimli::Section<gimli::EndianBuf<'input, Endian>>,
            Endian: gimli::Endianity,
            'file: 'input,
        {
            let data = file.section_data_by_name(S::section_name()).unwrap_or(&[]);
            S::from(gimli::EndianBuf::new(data, endian))
        }

        let debug_abbrev: gimli::DebugAbbrev<_> = load_section(file, endian);
        let debug_info: gimli::DebugInfo<_> = load_section(file, endian);
        let debug_line: gimli::DebugLine<_> = load_section(file, endian);
        let debug_ranges: gimli::DebugRanges<_> = load_section(file, endian);
        let debug_str: gimli::DebugStr<_> = load_section(file, endian);

        let mut unit_ranges = Vec::new();
        let mut res_units = Vec::new();
        let mut units = debug_info.units();
        while let Some(dw_unit) = units.next()? {
            let unit_id = res_units.len();

            let abbrevs = dw_unit.abbreviations(&debug_abbrev)?;

            let dlr;
            let dcd;
            let dcn;
            let base_addr;
            let lang;
            {
                let mut cursor = dw_unit.entries(&abbrevs);

                let unit = match cursor.next_dfs()? {
                    Some((_, unit)) if unit.tag() == gimli::DW_TAG_compile_unit => unit,
                    _ => continue, // wtf?
                };

                dlr = match unit.attr_value(gimli::DW_AT_stmt_list)? {
                    Some(gimli::AttributeValue::DebugLineRef(dlr)) => dlr,
                    _ => unreachable!(),
                };
                dcd = unit.attr(gimli::DW_AT_comp_dir)?
                    .and_then(|x| x.string_value(&debug_str));
                dcn = unit.attr(gimli::DW_AT_name)?
                    .and_then(|x| x.string_value(&debug_str));
                base_addr = match unit.attr_value(gimli::DW_AT_low_pc)? {
                    Some(gimli::AttributeValue::Addr(addr)) => addr,
                    None => 0, // ThinLTO yields inline-only compilation units; this is valid
                    _ => unreachable!(),
                };
                lang = match unit.attr_value(gimli::DW_AT_language)? {
                    Some(gimli::AttributeValue::Language(lang)) => Some(lang),
                    _ => None,
                };
                if let Some(mut ranges) =
                    read_ranges(unit, &debug_ranges, dw_unit.address_size(), base_addr)?
                {
                    while let Some(range) = ranges.next()? {
                        if range.begin == range.end {
                            continue;
                        }

                        unit_ranges.push((range, unit_id));
                    }
                }
            }

            res_units.push(ResUnit {
                dw_unit,
                abbrevs,
                line_offset: dlr,
                comp_dir: dcd,
                comp_name: dcn,
                lang,
                base_addr,
                lines: Lazy::new(),
                funcs: Lazy::new(),
            });
        }

        unit_ranges.sort_by_key(|x| x.0.begin);

        // Ranges need to be disjoint so that we can binary search, but weak symbols can
        // cause overlap. In this case, we don't care which unit is used, so ignore the
        // beginning of the subseqent range to avoid overlap.
        let mut prev_end = 0;
        for range in &mut unit_ranges {
            if range.0.begin < prev_end {
                range.0.begin = prev_end;
            }
            if range.0.end < prev_end {
                range.0.end = prev_end;
            } else {
                prev_end = range.0.end;
            }
        }
        debug_assert!(unit_ranges.windows(2).all(|w| w[0].0.end <= w[1].0.begin));

        Ok(Context {
            units: res_units,
            unit_ranges,
            sections: DebugSections {
                debug_str,
                debug_ranges,
                debug_line,
            },
        })
    }
}

impl<R> ResUnit<R>
where
    R: gimli::Reader + Sync,
    R::Offset: Sync,
{
    fn parse_lines(&self, sections: &DebugSections<R>) -> Result<&Lines<R>, Error> {
        self.lines
            .get_or_create(|| {
                let ilnp = sections.debug_line.program(
                    self.line_offset,
                    self.dw_unit.address_size(),
                    self.comp_dir.clone(),
                    self.comp_name.clone(),
                )?;

                let (lnp, mut sequences) = ilnp.sequences()?;
                sequences.retain(|x| x.start != 0);
                sequences.sort_by_key(|x| x.start);
                Ok(Lines { lnp, sequences })
            })
            .as_ref()
            .map_err(Error::clone)
    }

    fn parse_functions(
        &self,
        sections: &DebugSections<R>,
    ) -> Result<&IntervalTree<u64, Func<R::Offset>>, Error> {
        self.funcs
            .get_or_create(|| {
                let mut results = Vec::new();
                let mut depth = 0;
                let mut cursor = self.dw_unit.entries(&self.abbrevs);
                while let Some((d, entry)) = cursor.next_dfs()? {
                    depth += d;
                    match entry.tag() {
                        gimli::DW_TAG_subprogram | gimli::DW_TAG_inlined_subroutine => {
                            // may be an inline-only function and thus not have any ranges
                            if let Some(mut ranges) = read_ranges(
                                entry,
                                &sections.debug_ranges,
                                self.dw_unit.address_size(),
                                self.base_addr,
                            )? {
                                while let Some(range) = ranges.next()? {
                                    // Ignore invalid DWARF so that a query of 0 does not give
                                    // a long list of matches.
                                    // TODO: don't ignore if there is a section at this address
                                    if range.begin == 0 {
                                        continue;
                                    }
                                    results.push(Element {
                                        range: range.begin..range.end,
                                        value: Func {
                                            entry_off: entry.offset(),
                                            depth,
                                        },
                                    });
                                }
                            }
                        }
                        _ => (),
                    }
                }

                let tree: IntervalTree<_, _> = results.into_iter().collect();
                Ok(tree)
            })
            .as_ref()
            .map_err(Error::clone)
    }
}

struct DebugSections<R: gimli::Reader> {
    debug_str: gimli::DebugStr<R>,
    debug_ranges: gimli::DebugRanges<R>,
    debug_line: gimli::DebugLine<R>,
}

pub struct IterFrames<'ctx, R>
where
    R: gimli::Reader + Sync + 'ctx,
    R::Offset: Sync,
{
    unit_id: usize,
    units: &'ctx Vec<ResUnit<R>>,
    sections: &'ctx DebugSections<R>,
    funcs: smallvec::IntoIter<[&'ctx Func<R::Offset>; 16]>,
    next: Option<Location>,
}

pub struct Frame<R: gimli::Reader> {
    pub function: Option<FunctionName<R>>,
    pub location: Option<Location>,
}

pub struct FunctionName<R: gimli::Reader> {
    name: R,
    pub language: Option<gimli::DwLang>,
}

impl<R: gimli::Reader> FunctionName<R> {
    pub fn raw_name(&self) -> Result<Cow<str>, Error> {
        self.name.to_string_lossy()
    }
}

pub fn demangle(name: &str, language: gimli::DwLang) -> Option<String> {
    match language {
        #[cfg(feature = "rustc-demangle")]
        gimli::DW_LANG_Rust => rustc_demangle::try_demangle(name)
            .ok()
            .as_ref()
            .map(ToString::to_string),
        #[cfg(feature = "cpp_demangle")]
        gimli::DW_LANG_C_plus_plus
        | gimli::DW_LANG_C_plus_plus_03
        | gimli::DW_LANG_C_plus_plus_11
        | gimli::DW_LANG_C_plus_plus_14 => cpp_demangle::Symbol::new(name)
            .ok()
            .and_then(|x| x.demangle(&Default::default()).ok()),
        _ => None,
    }
}

enum WrapRangeIter<R: gimli::Reader> {
    Real(gimli::RangesIter<R>),
    Synthetic(Option<gimli::Range>),
}

impl<R: gimli::Reader> FallibleIterator for WrapRangeIter<R> {
    type Item = gimli::Range;
    type Error = gimli::Error;

    fn next(&mut self) -> Result<Option<gimli::Range>, gimli::Error> {
        match *self {
            WrapRangeIter::Real(ref mut ri) => ri.next(),
            WrapRangeIter::Synthetic(ref mut range) => Ok(range.take()),
        }
    }
}

pub struct Location {
    pub file: Option<PathBuf>,
    pub line: Option<u64>,
    pub column: Option<u64>,
}

impl<R> Context<R>
where
    R: gimli::Reader + Sync,
    R::Offset: Sync,
{
    fn find_unit(&self, probe: u64) -> Option<usize> {
        let idx = self.unit_ranges.binary_search_by(|r| {
            if probe < r.0.begin {
                Ordering::Greater
            } else if probe >= r.0.end {
                Ordering::Less
            } else {
                Ordering::Equal
            }
        });
        let idx = match idx {
            Ok(x) => x,
            Err(_) => return None,
        };

        let (_, unit_id) = self.unit_ranges[idx];
        Some(unit_id)
    }

    pub fn find_location(&self, probe: u64) -> Result<Option<Location>, Error> {
        match self.find_unit(probe) {
            Some(unit_id) => self.units[unit_id].find_location(probe, &self.sections),
            None => Ok(None),
        }
    }

    pub fn find_frames(&self, probe: u64) -> Result<IterFrames<R>, Error> {
        let (unit_id, loc, funcs) = match self.find_unit(probe) {
            Some(unit_id) => {
                let unit = &self.units[unit_id];
                let loc = unit.find_location(probe, &self.sections)?;
                let funcs = unit.parse_functions(&self.sections)?;
                let mut res: SmallVec<[_; 16]> =
                    funcs.query_point(probe).map(|x| &x.value).collect();
                res.sort_by_key(|x| -x.depth);
                (unit_id, loc, res)
            }
            None => (0, None, SmallVec::new()),
        };

        Ok(IterFrames {
            unit_id,
            units: &self.units,
            sections: &self.sections,
            funcs: funcs.into_iter(),
            next: loc,
        })
    }
}

impl<R> ResUnit<R>
where
    R: gimli::Reader + Sync,
    R::Offset: Sync,
{
    fn find_location(
        &self,
        probe: u64,
        sections: &DebugSections<R>,
    ) -> Result<Option<Location>, Error> {
        let lines = self.parse_lines(sections)?;
        let idx = lines.sequences.binary_search_by(|ln| {
            if probe < ln.start {
                Ordering::Greater
            } else if probe >= ln.end {
                Ordering::Less
            } else {
                Ordering::Equal
            }
        });
        let idx = match idx {
            Ok(x) => x,
            Err(_) => return Ok(None),
        };
        let ln = &lines.sequences[idx];
        let mut sm = lines.lnp.resume_from(ln);
        let mut file = None;
        let mut line = None;
        let mut column = None;
        while let Some((_, row)) = sm.next_row()? {
            if row.address() > probe {
                break;
            }

            file = row.file(lines.lnp.header());
            line = row.line();
            column = match row.column() {
                gimli::ColumnType::LeftEdge => None,
                gimli::ColumnType::Column(x) => Some(x),
            };
        }

        let file = match file {
            Some(file) => Some(self.render_file(file, lines)?),
            None => None,
        };

        Ok(Some(Location { file, line, column }))
    }

    fn render_file(
        &self,
        file: &gimli::FileEntry<R>,
        lines: &Lines<R>,
    ) -> Result<PathBuf, gimli::Error> {
        let mut path = if let Some(ref comp_dir) = self.comp_dir {
            PathBuf::from(comp_dir.to_string_lossy()?.as_ref())
        } else {
            PathBuf::new()
        };

        if let Some(directory) = file.directory(lines.lnp.header()) {
            path.push(directory.to_string_lossy()?.as_ref());
        }

        path.push(file.path_name().to_string_lossy()?.as_ref());

        Ok(path)
    }
}

type Error = gimli::Error;

fn name_attr<'abbrev, 'unit, R>(
    entry: &gimli::DebuggingInformationEntry<'abbrev, 'unit, R, R::Offset>,
    unit: &ResUnit<R>,
    sections: &DebugSections<R>,
    units: &[ResUnit<R>],
    recursion_limit: usize,
) -> Result<Option<R>, Error>
where
    R: gimli::Reader + Sync,
    R::Offset: Sync,
{
    if recursion_limit == 0 {
        return Ok(None);
    }

    if let Some(attr) = entry.attr(gimli::DW_AT_linkage_name)? {
        if let Some(val) = attr.string_value(&sections.debug_str) {
            return Ok(Some(val));
        }
    }
    if let Some(attr) = entry.attr(gimli::DW_AT_MIPS_linkage_name)? {
        if let Some(val) = attr.string_value(&sections.debug_str) {
            return Ok(Some(val));
        }
    }
    if let Some(attr) = entry.attr(gimli::DW_AT_name)? {
        if let Some(val) = attr.string_value(&sections.debug_str) {
            return Ok(Some(val));
        }
    }

    let next = entry
        .attr_value(gimli::DW_AT_abstract_origin)?
        .or(entry.attr_value(gimli::DW_AT_specification)?);
    match next {
        Some(gimli::AttributeValue::UnitRef(offset)) => {
            let mut entries = unit.dw_unit.entries_at_offset(&unit.abbrevs, offset)?;
            if let Some((_, entry)) = entries.next_dfs()? {
                return name_attr(entry, unit, sections, units, recursion_limit - 1);
            } else {
                return Err(gimli::Error::NoEntryAtGivenOffset);
            }
        }
        Some(gimli::AttributeValue::DebugInfoRef(dr)) => if let Some((unit, offset)) = units
            .iter()
            .filter_map(|unit| dr.to_unit_offset(&unit.dw_unit).map(|uo| (unit, uo)))
            .next()
        {
            let mut entries = unit.dw_unit.entries_at_offset(&unit.abbrevs, offset)?;
            if let Some((_, entry)) = entries.next_dfs()? {
                return name_attr(entry, unit, sections, units, recursion_limit - 1);
            }
        } else {
            return Err(gimli::Error::NoEntryAtGivenOffset);
        },
        _ => {}
    }

    Ok(None)
}

impl<'ctx, R> FallibleIterator for IterFrames<'ctx, R>
where
    R: gimli::Reader + Sync + 'ctx,
    R::Offset: Sync,
{
    type Item = Frame<R>;
    type Error = Error;
    fn next(&mut self) -> Result<Option<Frame<R>>, Error> {
        let (loc, func) = match (self.next.take(), self.funcs.next()) {
            (None, None) => return Ok(None),
            (loc, Some(func)) => (loc, func),
            (Some(loc), None) => {
                return Ok(Some(Frame {
                    function: None,
                    location: Some(loc),
                }))
            }
        };

        let unit = &self.units[self.unit_id];

        let mut cursor = unit.dw_unit
            .entries_at_offset(&unit.abbrevs, func.entry_off)?;
        let (_, entry) = cursor
            .next_dfs()?
            .expect("DIE we read a while ago is no longer readable??");

        // Set an arbitrary recursion limit of 16
        let name = name_attr(entry, unit, self.sections, self.units, 16)?;

        if entry.tag() == gimli::DW_TAG_inlined_subroutine {
            let file = match entry.attr_value(gimli::DW_AT_call_file)? {
                Some(gimli::AttributeValue::FileIndex(fi)) => {
                    let lines = unit.parse_lines(self.sections)?;
                    match lines.lnp.header().file(fi) {
                        Some(file) => Some(unit.render_file(file, lines)?),
                        None => None,
                    }
                }
                _ => None,
            };

            let line = entry
                .attr(gimli::DW_AT_call_line)?
                .and_then(|x| x.udata_value())
                .and_then(|x| if x == 0 { None } else { Some(x) });
            let column = entry
                .attr(gimli::DW_AT_call_column)?
                .and_then(|x| x.udata_value());

            self.next = Some(Location { file, line, column });
        }


        Ok(Some(Frame {
            function: name.map(|name| {
                FunctionName {
                    name,
                    language: unit.lang,
                }
            }),
            location: loc,
        }))
    }
}
