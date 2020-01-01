//! # SPIR-Q: Light Weight SPIR-V Query Utility for Graphics.
//!
//! SPIR-Q is a light weight library for SPIR-V pipeline metadata query, which
//! can be very useful for dynamic graphics/compute pipeline construction,
//! shader debugging and so on. SPIR-Q is currently compatible with a subset of
//! SPIR-V 1.5, with most of graphics capabilities but no OpenCL kernel
//! capabilities covered.
mod consts;
mod parse;
mod instr;
mod sym;
pub mod reflect;
pub mod error;
pub mod ty;

use std::convert::TryInto;
use std::collections::{HashMap};
use std::fmt;
use std::iter::FromIterator;
use std::ops::Deref;
use parse::{Instrs, Instr};
use ty::{Type, DescriptorType};
pub use sym::*;
pub use error::{Error, Result};
pub use spirv_headers::ExecutionModel;

/// SPIR-V program binary.
#[derive(Debug, Default, Clone)]
pub struct SpirvBinary(Vec<u32>);
impl From<Vec<u32>> for SpirvBinary {
    fn from(x: Vec<u32>) -> Self { SpirvBinary(x) }
}
impl FromIterator<u32> for SpirvBinary {
    fn from_iter<I: IntoIterator<Item=u32>>(iter: I) -> Self { SpirvBinary(iter.into_iter().collect::<Vec<u32>>()) }
}
impl From<Vec<u8>> for SpirvBinary {
    fn from(x: Vec<u8>) -> Self {
        if x.len() == 0 { return SpirvBinary::default(); }
        x.chunks_exact(4)
            .map(|x| x.try_into().unwrap())
            .map(match x[0] {
                0x03 => u32::from_le_bytes,
                0x07 => u32::from_be_bytes,
                _ => return SpirvBinary::default(),
            })
            .collect::<SpirvBinary>()
    }
}

impl SpirvBinary {
    pub(crate) fn instrs<'a>(&'a self) -> Instrs<'a> { Instrs::new(&self.0) }
    pub fn reflect(&self) -> Result<Box<[EntryPoint]>> {
        reflect::reflect_spirv(&self)
    }
    pub fn words(&self) -> &[u32] {
        &self.0
    }
    pub fn bytes(&self) -> &[u8] {
        unsafe {
            let len = self.0.len() * std::mem::size_of::<u32>();
            let ptr = self.0.as_ptr() as *const u8;
            std::slice::from_raw_parts(ptr, len)
        }
    }
    pub fn into_words(self) -> Vec<u32> { self.0 }
}


/// Internal hasher for type equality check.
pub(crate) fn hash<H: std::hash::Hash>(h: &H) -> u64 {
    use std::hash::Hasher;
    use std::collections::hash_map::DefaultHasher;
    let mut hasher = DefaultHasher::new();
    h.hash(&mut hasher);
    hasher.finish()
}


// Resource locationing.

/// Interface variable location.
pub type Location = u32;
/// Descriptor set and binding point carrier.
#[derive(PartialEq, Eq, Hash, Default, Clone, Copy)]
pub struct DescriptorBinding(Option<(u32, u32)>);
impl DescriptorBinding {
    pub fn push_const() -> Self { DescriptorBinding(None) }
    pub fn desc_bind(desc_set: u32, bind_point: u32) -> Self { DescriptorBinding(Some((desc_set, bind_point))) }

    pub fn is_push_const(&self) -> bool { self.0.is_none() }
    pub fn is_desc_bind(&self) -> bool { self.0.is_some() }
    pub fn into_inner(self) -> Option<(u32, u32)> { self.0 }
}
impl fmt::Debug for DescriptorBinding {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        if let Some((set, bind)) = self.0 {
            write!(f, "(set={}, bind={})", set, bind)
        } else {
            write!(f, "(push_constant)")
        }
    }
}

#[derive(PartialEq, Eq, Hash, Clone, Copy)]
pub(crate) enum ResourceLocator {
    Input(Location),
    Output(Location),
    Descriptor(DescriptorBinding),
}

// Resolution results.


/// Interface variables resolution result.
#[derive(Debug)]
pub struct InterfaceVariableResolution<'a> {
    /// Location of the current interface variable. It should be noted that
    /// matrix types can take more than one location.
    pub location: Location,
    /// Type of the resolution target.
    pub ty: &'a Type,
}

/// Descriptor variable resolution result.
#[derive(Debug)]
pub struct DescriptorResolution<'a> {
    /// Descriptor set and binding point of the descriptor.
    pub desc_bind: DescriptorBinding,
    /// Type of the descriptor.
    pub desc_ty: &'a DescriptorType,
    /// Resolution of a variable in the descriptor, if the resolution doesn't
    /// end at a descriptor type.
    pub member_var_res: Option<MemberVariableResolution<'a>>,
}
#[derive(Debug)]
pub struct MemberVariableResolution<'a> {
    /// Offset to the resolution target from the beginning of buffer.
    pub offset: usize,
    /// Type of the resolution target.
    pub ty: &'a Type,
}

/// A set of information used to describe variable typing and routing.
#[derive(Default, Clone)]
pub struct Manifest {
    pub(crate) input_map: HashMap<Location, Type>,
    pub(crate) output_map: HashMap<Location, Type>,
    pub(crate) desc_map: HashMap<DescriptorBinding, DescriptorType>,
    pub(crate) var_name_map: HashMap<String, ResourceLocator>,
}
impl Manifest {
    /// Merge metadata records in another manifest into the current one.
    pub fn merge(&mut self, other: &Manifest) -> Result<()> {
        use std::collections::hash_map::Entry::{Vacant, Occupied};
        for (location, ivar_ty) in other.input_map.iter() {
            match self.input_map.entry(*location) {
                Vacant(entry) => { entry.insert(ivar_ty.clone()); },
                Occupied(entry) => if hash(entry.get()) != hash(&ivar_ty) {
                    return Err(Error::MismatchedManifest);
                }
            }
        }
        for (location, ivar_ty) in other.output_map.iter() {
            match self.output_map.entry(*location) {
                Vacant(entry) => { entry.insert(ivar_ty.clone()); },
                Occupied(entry) => if hash(entry.get()) != hash(&ivar_ty) {
                    return Err(Error::MismatchedManifest);
                }
            }
        }
        for (desc_bind, desc_ty) in other.desc_map.iter() {
            match self.desc_map.entry(*desc_bind) {
                Vacant(entry) => { entry.insert(desc_ty.clone()); },
                Occupied(mut entry) => {
                    if let DescriptorType::PushConstant(Type::Struct(dst_struct_ty)) = entry.get_mut() {
                        // Merge push constants scattered in different stages.
                        // This match must success.
                        if let DescriptorType::PushConstant(Type::Struct(src_struct_ty)) = desc_ty {
                            dst_struct_ty.merge(&src_struct_ty)?;
                        } else {
                            unreachable!("push constant merging with push constant");
                        }
                        // It's guaranteed to be interface uniform so we don't
                        // have to check that.
                    } else {
                        // Just regular descriptor types. Simply compare the
                        // hashes.
                        if hash(entry.get()) != hash(&desc_ty) {
                            return Err(Error::MismatchedManifest);
                        }
                    }
                }
            }
        }
        for (name, locator) in other.var_name_map.iter() {
            match self.var_name_map.entry(name.to_owned()) {
                Vacant(entry) => { entry.insert(locator.clone()); },
                Occupied(entry) => if entry.get() != locator {
                    // Mismatched names are not allowed.
                    return Err(Error::MismatchedManifest);
                },
            }
        }
        Ok(())
    }
    /// Get the input interface variable type.
    pub fn get_input(&self, location: Location) -> Option<Type> {
        self.input_map.get(location)
    }
    /// Get the output interface variable type.
    pub fn get_output(&self, location: Location) -> Option<Type> {
        self.output_map.get(location)
    }
    /// Get the descriptor type at the given descriptor binding point.
    pub fn get_desc(&self, desc_bind: DescriptorBinding) -> Option<DescriptorType> {
        self.desc_map.get(desc_bind)
    }
    fn resolve_ivar<'a>(&self, map: &'a HashMap<Location, Type>, sym: &Sym) -> Option<InterfaceVariableResolution<'a>> {
        let mut segs = sym.segs();
        let location = match segs.next() {
            Some(Seg::Index(location)) => location as u32,
            Some(Seg::Name(name)) => {
                if let Some(ResourceLocator::Input(location)) = self.var_name_map.get(name) {
                    *location
                } else { return None }
            },
            _ => return None,
        };
        if segs.next().is_some() { return None }
        let ty = map.get(&location)?;
        let ivar_res = InterfaceVariableResolution { location, ty };
        Some(ivar_res)
    }
    /// Get the metadata of a input variable identified by a symbol.
    pub fn resolve_input(&self, sym: &Sym) -> Option<InterfaceVariableResolution> {
        self.resolve_ivar(&self.output_map, sym)
    }
    /// Get the metadata of a output variable identified by a symbol.
    pub fn resolve_output(&self, sym: &Sym) -> Option<InterfaceVariableResolution> {
        self.resolve_ivar(&self.input_map, sym)
    }
    /// Get the metadata of a descriptor variable identified by a symbol.
    /// If the exact variable cannot be resolved, the descriptor part of the
    /// resolution will still be returned, if possible.
    pub fn resolve_desc(&self, sym: &Sym) -> Option<DescriptorResolution> {
        let mut segs = sym.segs();
        let desc_bind = match segs.next() {
            Some(Seg::Index(desc_set)) => {
                if let Some(Seg::Index(bind_point)) = segs.next() {
                    DescriptorBinding::desc_bind(desc_set as u32, bind_point as u32)
                } else { return None; }
            },
            Some(Seg::Empty) => {
                // Symbols started with an empty head, like ".modelView", is
                // used to identify push constants.
                DescriptorBinding::push_const()
            }
            Some(Seg::Name(name)) => {
                if let Some(ResourceLocator::Descriptor(desc_bind)) = self.var_name_map.get(name) {
                    *desc_bind
                } else { return None; }
            },
            None => return None,
        };
        let desc_ty = self.desc_map.get(&desc_bind)?;
        let member_var_res = desc_ty.resolve(segs.remaining());
        let desc_res = DescriptorResolution { desc_bind, desc_ty, member_var_res };
        Some(desc_res)
    }
    /// List all input locations
    pub fn inputs<'a>(&'a self) -> impl Iterator<Item=InterfaceVariableResolution<'a>> {
        self.input_map.iter()
            .map(|(&location, ty)| {
                InterfaceVariableResolution { location, ty }
            })
    }
    /// List all output locations in this manifest.
    pub fn outputs<'a>(&'a self) -> impl Iterator<Item=InterfaceVariableResolution<'a>> {
        self.output_map.iter()
            .map(|(&location, ty)|  {
                InterfaceVariableResolution { location, ty }
            })
    }
    /// List all descriptors in this manifest. Results will not contain anything
    /// about exact variables in buffers.
    pub fn descs<'a>(&'a self) -> impl Iterator<Item=DescriptorResolution<'a>> {
        self.desc_map.iter()
            .map(|(&desc_bind, desc_ty)| {
                DescriptorResolution{ desc_bind, desc_ty, member_var_res: None }
            })
    }
}


// SPIR-V program entry points.

/// Representing an entry point described in a SPIR-V.
#[derive(Clone)]
pub struct EntryPoint {
    pub exec_model: ExecutionModel,
    pub name: String,
    pub manifest: Manifest,
}
impl Deref for EntryPoint {
    type Target = Manifest;
    fn deref(&self) -> &Self::Target { &self.manifest }
}
impl fmt::Debug for EntryPoint {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct(&self.name)
            .field("inputs", &self.manifest.input_map)
            .field("outputs", &self.manifest.output_map)
            .field("descriptors", &self.manifest.desc_map)
            .finish()
    }
}
