extern crate object;
extern crate intervaltree;
extern crate fallible_iterator;
extern crate gimli;
extern crate smallvec;
#[cfg(feature = "rustc-demangle")]
extern crate rustc_demangle;
#[cfg(feature = "cpp_demangle")]
extern crate cpp_demangle;

use std::path::PathBuf;
use std::cmp::Ordering;
use std::borrow::Cow;
use std::fmt::{Display, Formatter, Result as FmtResult};
use std::u64;

use fallible_iterator::FallibleIterator;
use intervaltree::{IntervalTree, Element};
use smallvec::SmallVec;

struct Func<T> {
    unit_id: usize,
    entry_off: gimli::UnitOffset<T>,
    depth: isize,
}

struct ResUnit<R: gimli::Reader> {
    dw_unit: gimli::CompilationUnitHeader<R, R::Offset>,
    abbrevs: gimli::Abbreviations,
    inner: UnitInner<R>,
}

struct UnitInner<R: gimli::Reader> {
    lnp: gimli::CompleteLineNumberProgram<R>,
    sequences: Vec<gimli::LineNumberSequence<R>>,
    comp_dir: Option<R>,
    lang: gimli::DwLang,
    base_addr: u64,
}

fn render_file<R: gimli::Reader>(header: &gimli::LineNumberProgramHeader<R>,
                                 ffile: &gimli::FileEntry<R>,
                                 dcd: &Option<R>) -> Result<PathBuf, gimli::Error> {
    let mut path = if let &Some(ref dcd) = dcd {
        PathBuf::from(dcd.to_string_lossy()?.as_ref())
    } else {
        PathBuf::new()
    };

    if let Some(directory) = ffile.directory(header) {
        path.push(directory.to_string_lossy()?.as_ref());
    }

    path.push(ffile.path_name().to_string_lossy()?.as_ref());

    Ok(path)
}

pub struct Context<R: gimli::Reader> {
    unit_ranges: Vec<(gimli::Range, usize)>,
    units: Vec<ResUnit<R>>,
    sections: DebugSections<R>,
}

pub struct FullContext<R: gimli::Reader> {
    funcs: IntervalTree<u64, Func<R::Offset>>,
    light: Context<R>,
}

fn read_ranges<R: gimli::Reader>(entry: &gimli::DebuggingInformationEntry<R, R::Offset>,
                                 debug_ranges: &gimli::DebugRanges<R>,
                                 addr_size: u8, base_addr: u64) -> Result<Option<WrapRangeIter<R>>, Error> {
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
            WrapRangeIter::Synthetic(Some(gimli::Range { begin: low_pc, end: high_pc }))
        },
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

        fn load_section<'input, 'file, S, Endian>(file: &'file object::File<'input>, endian: Endian) -> S
            where S: gimli::Section<gimli::EndianBuf<'input, Endian>>, Endian: gimli::Endianity, 'file: 'input,
        {
            let data = file.get_section(S::section_name()).unwrap_or(&[]);
            S::from(gimli::EndianBuf::new(data, endian))
        }

        let debug_abbrev: gimli::DebugAbbrev<_> = load_section(file, endian);
        let debug_info: gimli::DebugInfo<_> = load_section(file, endian);
        let debug_line: gimli::DebugLine<_> = load_section(file, endian);
        let debug_ranges: gimli::DebugRanges<_> = load_section(file, endian);
        let debug_str: gimli::DebugStr<_> = load_section(file, endian);
        let debug_loc: gimli::DebugLoc<_> = load_section(file, endian);

        let mut unit_ranges = Vec::new();
        let mut res_units = Vec::new();
        let mut units = debug_info.units();
        while let Some(dw_unit) = units.next()? {
            let unit_id = res_units.len();

            let abbrevs = dw_unit.abbreviations(&debug_abbrev)?;

            let inner = {
                let mut cursor = dw_unit.entries(&abbrevs);

                let unit = match cursor.next_dfs()? {
                    Some((_, unit)) if unit.tag() == gimli::DW_TAG_compile_unit => unit,
                    _ => continue, // wtf?
                };

                let dlr = match unit.attr_value(gimli::DW_AT_stmt_list)? {
                    Some(gimli::AttributeValue::DebugLineRef(dlr)) => dlr,
                    _ => unreachable!(),
                };
                let dcd = unit.attr(gimli::DW_AT_comp_dir)?.and_then(|x| x.string_value(&debug_str));
                let dcn = unit.attr(gimli::DW_AT_name)?.and_then(|x| x.string_value(&debug_str));
                let base_addr = match unit.attr_value(gimli::DW_AT_low_pc)? {
                    Some(gimli::AttributeValue::Addr(addr)) => addr,
                    _ => continue, // no base addr? HOW???
                };
                let lang = match unit.attr_value(gimli::DW_AT_language)? {
                    Some(gimli::AttributeValue::Language(lang)) => lang,
                    _ => continue, // no language? HOW??? (TODO: this is not strictly mandatory for us)
                };
                if let Some(mut ranges) = read_ranges(unit, &debug_ranges, dw_unit.address_size(), base_addr)? {
                    while let Some(range) = ranges.next()? {
                        if range.begin == range.end { continue; }

                        unit_ranges.push((range, unit_id));
                    }
                }

                let ilnp = debug_line.program(dlr, dw_unit.address_size(), dcd, dcn)?;
                let (lnp, mut sequences) = ilnp.sequences()?;
                sequences.sort_by_key(|x| x.start);
                UnitInner { lnp, sequences, comp_dir: dcd, lang, base_addr }
            };

            res_units.push(ResUnit {
                dw_unit,
                abbrevs,
                inner,
            });
        }

        unit_ranges.sort_by_key(|x| x.0.begin);

        // ranges need to be disjoint or we lost
        debug_assert!(unit_ranges.windows(2).all(|w| w[0].0.end <= w[1].0.begin));

        Ok(Context {
            units: res_units,
            unit_ranges,
            sections: DebugSections {
                debug_str,
                debug_ranges,
                debug_loc,
            }
        })
    }
}

impl<R: gimli::Reader> Context<R> {
    pub fn parse_functions(self) -> Result<FullContext<R>, Error> {
        let mut results = Vec::new();

        for (unit_id, unit) in self.units.iter().enumerate() {
            let mut depth = 0;

            let dw_unit = &unit.dw_unit;
            let abbrevs = &unit.abbrevs;

            let mut cursor = dw_unit.entries(&abbrevs);
            while let Some((d, entry)) = cursor.next_dfs()? {
                depth += d;
                match entry.tag() {
                    gimli::DW_TAG_subprogram | gimli::DW_TAG_inlined_subroutine => {
                        // may be an inline-only function and thus not have any ranges
                        if let Some(mut ranges) = read_ranges(entry,
                                                              &self.sections.debug_ranges,
                                                              dw_unit.address_size(),
                                                              unit.inner.base_addr)? {
                            while let Some(range) = ranges.next()? {
                                results.push(Element {
                                    range: range.begin .. range.end,
                                    value: Func {
                                        unit_id,
                                        entry_off: entry.offset(),
                                        depth,
                                    }
                                });
                            }
                        }
                    }
                    _ => (),
                }
            }
        }

        let tree: IntervalTree<_, _> = results.into_iter().collect();
        Ok(FullContext {
            light: self,
            funcs: tree,
        })
    }
}

struct DebugSections<R: gimli::Reader> {
    debug_str: gimli::DebugStr<R>,
    debug_ranges: gimli::DebugRanges<R>,
    debug_loc: gimli::DebugLoc<R>,
}

pub struct IterFrames<'ctx, R: gimli::Reader + 'ctx> {
    units: &'ctx Vec<ResUnit<R>>,
    sections: &'ctx DebugSections<R>,
    funcs: smallvec::IntoIter<[&'ctx Func<R::Offset>; 16]>,
    next: Option<Location>,
}

pub struct Frame<'ctx, R: gimli::Reader + 'ctx> {
    pub function: Option<Function<'ctx, R>>,
    pub location: Option<Location>,
}

pub struct Function<'ctx, R: gimli::Reader + 'ctx> {
    cursor: gimli::EntriesCursor<'ctx, 'ctx, R>,
    sections: &'ctx DebugSections<R>,
    unit: &'ctx ResUnit<R>,
    name: R,
    pub language: gimli::DwLang,
}

impl<'ctx, R: gimli::Reader + 'ctx> Function<'ctx, R> {
    pub fn raw_name(&self) -> Result<Cow<str>, Error> {
        self.name.to_string_lossy()
    }

    pub fn demangled_name(&self) -> Result<Option<String>, Error> {
        let name = self.name.to_string_lossy()?;
        Ok(match self.language {
            #[cfg(feature = "rustc-demangle")]
            gimli::DW_LANG_Rust => rustc_demangle::try_demangle(name.as_ref())
                .ok().as_ref().map(ToString::to_string),
            #[cfg(feature = "cpp_demangle")]
            gimli::DW_LANG_C_plus_plus
                | gimli::DW_LANG_C_plus_plus_03
                | gimli::DW_LANG_C_plus_plus_11
                | gimli::DW_LANG_C_plus_plus_14 =>
                cpp_demangle::Symbol::new(name.as_ref()).ok()
                    .and_then(|x| x.demangle(&Default::default()).ok()),
            _ => None,
        })
    }

    pub fn stack_variables_at(&self, probe: u64) -> StackVarIter<'ctx, R> {
        StackVarIter {
            cursor: self.cursor.clone(),
            sections: self.sections,
            unit: self.unit,
            depth: 0,
            probe,
        }
    }
}

pub struct StackVarIter<'ctx, R: gimli::Reader + 'ctx> {
    cursor: gimli::EntriesCursor<'ctx, 'ctx, R>,
    sections: &'ctx DebugSections<R>,
    unit: &'ctx ResUnit<R>,
    depth: isize,
    probe: u64,
}

impl<'ctx, R: gimli::Reader + 'ctx> StackVarIter<'ctx, R> {
    fn next_dfs(&mut self) -> Result<Option<&gimli::DebuggingInformationEntry<'ctx, 'ctx, R, R::Offset>>, gimli::Error> {
        match self.cursor.next_dfs()? {
            Some((d, e)) => {
                self.depth += d;
                if self.depth > 0 {
                    Ok(Some(e))
                } else {
                    Ok(None)
                }
            }
            None => Ok(None)
        }
    }

    fn next_scoped_sibling(&mut self) -> Result<Option<&gimli::DebuggingInformationEntry<'ctx, 'ctx, R, R::Offset>>, gimli::Error> {
        Ok(if self.depth == 0 {
            // only just starting
            self.next_dfs()?
        } else {
            let has_sibling = self.cursor.next_sibling()?.is_some();
            if has_sibling {
                Some(self.cursor.current().unwrap()) // hack to make borrowck happy
            } else {
                self.next_dfs()?
            }
        })
    }
}

enum StackVarIterationAction {
    Sibling,
    Recurse,
}

pub struct StackVar<R: gimli::Reader> {
    data: gimli::Expression<R>,
    name: Option<R>,
}

impl<'ctx, R: gimli::Reader + 'ctx> FallibleIterator for StackVarIter<'ctx, R> {
    type Item = StackVar<R>;
    type Error = Error;

    fn next(&mut self) -> Result<Option<StackVar<R>>, Error> {
        // to keep borrowck happy
        let probe = self.probe;
        let base_addr = self.unit.inner.base_addr;

        let mut action = StackVarIterationAction::Sibling;
        loop {
            let entry = match action {
                StackVarIterationAction::Sibling => self.next_scoped_sibling()?,
                StackVarIterationAction::Recurse => self.next_dfs()?,
            };

            action = match entry {
                Some(entry) => match entry.tag() {
                    gimli::DW_TAG_variable => {
                        // this may be a result!
                        match entry.attr_value(gimli::DW_AT_location)? {
                            Some(gimli::AttributeValue::DebugLocRef(lr)) => {
                                let mut locs = self.sections.debug_loc
                                    .locations(lr, self.unit.dw_unit.address_size(), base_addr)?;
                                let loc = locs.find(|l| l.range.begin <= probe && l.range.end > probe)?;
                                if let Some(loc) = loc {
                                    let data = loc.data;
                                    return Ok(Some(StackVar {
                                        data,
                                        name: str_attr(entry,
                                                       &self.unit.dw_unit,
                                                       &self.unit.abbrevs,
                                                       self.sections,
                                                       gimli::DW_AT_name)?,
                                    }));
                                }
                            }
                            _ => (),
                        }

                        // Failed to match, keep going
                        StackVarIterationAction::Sibling
                    }
                    gimli::DW_TAG_lexical_block => {
                        // need to recurse into this?
                        let ranges = read_ranges(entry,
                                                 &self.sections.debug_ranges,
                                                 self.unit.dw_unit.address_size(),
                                                 base_addr)?;
                        if let Some(mut ranges) = ranges {
                            if ranges.any(|r| r.begin <= probe && r.end > probe)? {
                                StackVarIterationAction::Recurse
                            } else {
                                // doesn't intersect, so we skip this
                                StackVarIterationAction::Sibling
                            }
                        } else {
                            // no ranges specified, so we recurse
                            StackVarIterationAction::Recurse
                        }
                    }
                    _ => {
                        // unknown tag, ignore and go next
                        StackVarIterationAction::Sibling
                    }
                },
                None => return Ok(None),
            };
        }
    }
}

impl<'ctx, R: gimli::Reader + 'ctx> Display for Function<'ctx, R> {
    fn fmt(&self, fmt: &mut Formatter) -> FmtResult {
        let name = self.demangled_name().unwrap().map(Cow::from)
            .unwrap_or(self.raw_name().unwrap());
        write!(fmt, "{}", name)
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

impl<R: gimli::Reader> Context<R> {
    pub fn find_location(&self, probe: u64) -> Result<Option<Location>, Error> {
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
            Err(_) => return Ok(None),
        };

        let (_, unit_id) = self.unit_ranges[idx];

        self.find_location_inner(probe, &self.units[unit_id].inner)
    }

    fn find_location_inner(&self, probe: u64, uunit: &UnitInner<R>) -> Result<Option<Location>, Error> {
        let cp = &uunit.lnp;
        let idx = uunit.sequences.binary_search_by(|ln| {
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
        let ln = &uunit.sequences[idx];
        let mut sm = cp.resume_from(&ln);
        let mut file = None;
        let mut line = None;
        let mut column = None;
        while let Some((_, row)) = sm.next_row()? {
            if row.address() > probe {
                break;
            }

            file = row.file(cp.header());
            line = row.line();
            column = match row.column() {
                gimli::ColumnType::LeftEdge => None,
                gimli::ColumnType::Column(x) => Some(x),
            };
        }

        let file = match file {
            Some(file) => Some(render_file(uunit.lnp.header(), file, &uunit.comp_dir)?),
            None => None,
        };

        Ok(Some(Location { file, line, column }))
    }
}

impl<R: gimli::Reader> FullContext<R> {
    pub fn query<'a>(&self, probe: u64) -> Result<IterFrames<R>, Error> {
        let ctx = &self.light;
        let mut res: SmallVec<[_; 16]> = self.funcs.query_point(probe).map(|x| &x.value).collect();
        res.sort_by_key(|x| -x.depth);

        let loc = match res.get(0) {
            Some(r) => {
                let uunit = &ctx.units[r.unit_id].inner;
                self.light.find_location_inner(probe, uunit)
            }
            None => self.light.find_location(probe),
        };

        Ok(IterFrames {
            units: &ctx.units,
            sections: &ctx.sections,
            funcs: res.into_iter(),
            next: loc?,
        })
    }
}

type Error = gimli::Error;

fn str_attr<'abbrev, 'unit, R: gimli::Reader>(entry: &gimli::DebuggingInformationEntry<'abbrev, 'unit, R, R::Offset>,
                                              dw_unit: &gimli::CompilationUnitHeader<R, R::Offset>,
                                              abbrevs: &gimli::Abbreviations,
                                              sections: &DebugSections<R>,
                                              name: gimli::DwAt) -> Result<Option<R>, Error> {
    Ok(match entry.attr(name)? {
        Some(x) => x.string_value(&sections.debug_str),
        None => {
            match entry.attr_value(gimli::DW_AT_abstract_origin)? {
                Some(gimli::AttributeValue::UnitRef(offset)) => {
                    let mut tcursor = dw_unit.entries_at_offset(&abbrevs, offset)?;
                    match tcursor.next_dfs()? {
                        // FIXME: evil dwarf can send us into an infinite loop here
                        Some((_, entry)) => str_attr(entry, dw_unit, abbrevs, sections, name)?,
                        None => None,
                    }
                }
                None => None,
                x => panic!("wat {:?}", x),
            }
        }
    })
}

impl<'ctx, R: gimli::Reader + 'ctx> FallibleIterator for IterFrames<'ctx, R> {
    type Item = Frame<'ctx, R>;
    type Error = Error;
    fn next(&mut self) -> Result<Option<Frame<'ctx, R>>, Error> {
        let (loc, func) = match (self.next.take(), self.funcs.next()) {
            (None, None) => return Ok(None),
            (loc, Some(func)) => (loc, func),
            (Some(loc), None) => return Ok(Some(Frame {
                function: None,
                location: Some(loc),
            })),
        };

        let unit = &self.units[func.unit_id];

        let mut cursor = unit.dw_unit.entries_at_offset(&unit.abbrevs, func.entry_off)?;

        let name = { // I can't wait for non-lexical lifetimes
            let (_, entry) = cursor.next_dfs()?.expect("DIE we read a while ago is no longer readable??");

            let name = str_attr(entry, &unit.dw_unit, &unit.abbrevs, self.sections, gimli::DW_AT_linkage_name)?;

            if entry.tag() == gimli::DW_TAG_inlined_subroutine {
                let file = match entry.attr_value(gimli::DW_AT_call_file)? {
                    Some(gimli::AttributeValue::FileIndex(fi)) => {
                        if let Some(file) = unit.inner.lnp.header().file(fi) {
                            Some(render_file(unit.inner.lnp.header(), file, &unit.inner.comp_dir)?)
                        } else {
                            None
                        }
                    }
                    _ => None,
                };

                let line = entry.attr(gimli::DW_AT_call_line)?.and_then(|x| x.udata_value());
                let column = entry.attr(gimli::DW_AT_call_column)?.and_then(|x| x.udata_value());

                self.next = Some(Location { file, line, column });
            }

            name
        };


        Ok(Some(Frame {
            function: name.map(|name| Function {
                name,
                language: unit.inner.lang,
                cursor,
                sections: self.sections,
                unit,
            }),
            location: loc,
        }))
    }
}
