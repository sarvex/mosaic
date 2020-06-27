//! Intermediate representations.
//!
//! This module contains all the semantics we can represent in our bindings.
//!
//! It is split into two IRs, `cc` and `rs`, representing C++ and Rust
//! respectively. While the two IRs represent the same concepts, they imply a
//! different set of semantics (and, in some cases, idioms). The process of
//! converting between IRs contains explicit checks that the semantics in one
//! language IR can be represented in the other.

use crate::diagnostics::{err, ok, Diagnostic, Outcome, Span};
use crate::libclang::AstMethods;
use std::collections::{HashSet, VecDeque};
use std::num::NonZeroU16;
use std::{fmt, iter};

#[salsa::query_group(IrMethodsStorage)]
pub trait IrMethods {
    #[salsa::interned]
    fn intern_def(&self, def: DefKind) -> Def;
}

/// A top-level defintion of some kind.
///
/// A Def can be defined in either C++ or Rust.
#[derive(Clone, Debug, Hash, Eq, PartialEq)]
pub enum DefKind {
    CcDef(cc::ItemKind),
}
impl From<cc::ItemKind> for DefKind {
    fn from(item: cc::ItemKind) -> Self {
        DefKind::CcDef(item)
    }
}
impl From<cc::StructId> for DefKind {
    fn from(item: cc::StructId) -> Self {
        DefKind::CcDef(item.into())
    }
}

intern_key!(Def);
impl Def {
    pub fn lookup(&self, db: &impl IrMethods) -> DefKind {
        db.lookup_intern_def(*self)
    }
}

#[derive(Default, Debug, Eq, PartialEq)]
pub struct Module {
    pub exports: Vec<DefKind>,
}
impl Module {
    pub fn reachable_items<'db>(
        &self,
        db: &'db (impl IrMethods + AstMethods),
    ) -> impl Iterator<Item = DefKind> + 'db {
        let queue = self.exports.iter().cloned().collect();
        ReachableIter { db, queue }
    }

    pub fn to_rs_bindings(
        &self,
        db: &(impl IrMethods + cc::RsIr + AstMethods),
    ) -> Outcome<rs::Module> {
        self.reachable_items(db)
            .map(|def| {
                let item = match def {
                    DefKind::CcDef(cc::ItemKind::Struct(st)) => db.rs_struct_from_cc(st),
                };
                item.map(|i| (def, i))
            })
            .collect::<Outcome<Vec<_>>>()
            .then(|structs| {
                let mut exports = HashSet::new();
                let items = structs
                    .iter()
                    .map(|(def, st)| {
                        let item = rs::ItemKind::Struct(*st);
                        if self.exports.contains(def) {
                            exports.insert(item);
                        }
                        item
                    })
                    .collect();
                ok(rs::Module { items, exports })
            })
    }
}

struct ReachableIter<'db, DB: IrMethods + AstMethods> {
    db: &'db DB,
    queue: VecDeque<DefKind>,
}
impl<'db, DB: IrMethods + AstMethods> Iterator for ReachableIter<'db, DB> {
    type Item = DefKind;
    fn next<'a>(&'a mut self) -> Option<Self::Item> {
        let item = self.queue.pop_front()?;
        struct ReachableVisitor<'a>(&'a mut VecDeque<DefKind>);
        impl<'a, DB: IrMethods + AstMethods> Visitor<DB> for ReachableVisitor<'a> {
            fn visit_item(&mut self, _db: &DB, item: &DefKind) {
                debug_assert!(!self.0.contains(item));
                self.0.push_back(item.clone());
            }
        }
        ReachableVisitor(&mut self.queue).super_visit_item(self.db, &item);
        Some(item)
    }
}

trait Visitor<DB: IrMethods + AstMethods> {
    //fn visit_def(&mut self, db: &DB, def: Def) {
    //    self.super_visit_def(db, def);
    //}

    fn super_visit_def(&mut self, db: &DB, def: Def) {
        self.visit_item(db, &def.lookup(db));
    }

    fn visit_item(&mut self, db: &DB, item: &DefKind) {
        self.super_visit_item(db, item);
    }

    fn super_visit_item(&mut self, db: &DB, item: &DefKind) {
        match item {
            DefKind::CcDef(cc_item) => self.visit_cc_item(db, cc_item),
        }
    }

    fn visit_cc_item(&mut self, db: &DB, item: &cc::ItemKind) {
        self.super_visit_cc_item(db, item);
    }

    fn super_visit_cc_item(&mut self, db: &DB, item: &cc::ItemKind) {
        match item {
            cc::ItemKind::Struct(id) => self.visit_cc_struct(db, *id),
        }
    }

    fn visit_cc_struct(&mut self, db: &DB, id: cc::StructId) {
        self.super_visit_cc_struct(db, &id.lookup(db));
    }

    fn super_visit_cc_struct(&mut self, db: &DB, st: &cc::Struct) {
        #[allow(unused)]
        let cc::Struct {
            name,
            fields,
            offsets,
            methods,
            size,
            align,
            span,
        } = st;
        for field in fields {
            self.visit_cc_type_ref(db, field.ty.clone())
        }
    }

    fn visit_cc_type_ref(&mut self, db: &DB, ty_ref: cc::TypeRef) {
        self.super_visit_cc_type_ref(db, ty_ref);
    }

    fn super_visit_cc_type_ref(&mut self, db: &DB, ty_ref: cc::TypeRef) {
        self.visit_cc_type(db, &ty_ref.as_cc(db).skip_errs());
    }

    fn visit_cc_type(&mut self, db: &DB, ty: &cc::Ty) {
        self.super_visit_cc_type(db, ty);
    }

    fn super_visit_cc_type(&mut self, db: &DB, ty: &cc::Ty) {
        use cc::Ty::*;
        match ty {
            Error => (),
            Void => (),
            Float | Double => (),
            Short | UShort | Int | UInt | Long | ULong | LongLong | ULongLong | CharS | CharU
            | SChar | UChar | Size | SSize | PtrDiff => (),
            Bool => (),
            Struct(id) => self.visit_item(db, &DefKind::CcDef(cc::ItemKind::Struct(*id))),
        }
    }
}

/// Types and utilities used from both the Rust and C++ IRs.
mod common {
    use super::*;
    use crate::libclang;

    /// A C++ unqualified identifier.
    ///
    /// Examples: `std`, `vector`, or `MyClass`.
    #[derive(Clone, Debug, Hash, Eq, PartialEq)]
    pub struct Ident {
        s: String,
    }
    impl Ident {
        #[allow(dead_code)]
        pub fn as_str(&self) -> &str {
            &self.s
        }
    }
    impl From<&str> for Ident {
        /// Creates an identifier. Can panic if the identifier is invalid.
        fn from(id: &str) -> Ident {
            assert!(!id.contains("::"), "invalid identifier `{}`", id);
            Ident { s: id.to_string() }
        }
    }
    impl From<String> for Ident {
        fn from(id: String) -> Ident {
            From::from(id.as_str())
        }
    }
    impl fmt::Display for Ident {
        fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
            write!(f, "{}", self.s)
        }
    }

    /// A C++ fully-qualified name.
    ///
    /// Example: `std::vector`.
    #[derive(Clone, Hash, Eq, PartialEq)]
    pub struct Path {
        components: Vec<Ident>,
    }
    impl From<&str> for Path {
        fn from(mut path: &str) -> Path {
            if path.starts_with("::") {
                path = &path[2..];
            }
            Path {
                components: path.split("::").map(Ident::from).collect(),
            }
        }
    }
    impl From<String> for Path {
        fn from(path: String) -> Path {
            From::from(path.as_str())
        }
    }
    impl iter::FromIterator<Ident> for Path {
        fn from_iter<T: IntoIterator<Item = Ident>>(iter: T) -> Path {
            Path {
                components: iter.into_iter().collect(),
            }
        }
    }
    impl Path {
        pub fn iter(&self) -> impl Iterator<Item = &Ident> {
            self.components.iter()
        }
    }
    impl fmt::Display for Path {
        fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
            let mut iter = self.components.iter();
            let mut next = iter.next();
            while let Some(id) = next {
                write!(f, "{}", id)?;
                next = iter.next();
                if next.is_some() {
                    write!(f, "::")?;
                }
            }
            Ok(())
        }
    }
    impl fmt::Debug for Path {
        fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
            write!(f, "{}", self)
        }
    }

    pub type Offset = u16;

    pub(super) fn align_to(off: Offset, align: Align) -> Offset {
        let align = align.get();
        ((off + (align - 1)) / align) * align
    }

    #[derive(Copy, Clone, Debug, Eq, PartialEq, Hash, Ord, PartialOrd)]
    pub struct Align(NonZeroU16);

    impl Align {
        pub fn new(align: u16) -> Align {
            Align(NonZeroU16::new(align).expect("alignment must be nonzero"))
        }

        fn get(&self) -> u16 {
            self.0.get()
        }
    }

    impl fmt::Display for Align {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(f, "{}", self.0)
        }
    }

    // TODO u16 is probably not big enough for all cases
    #[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
    pub struct Size(pub(super) u16);

    impl Size {
        pub fn new(size: u16) -> Size {
            Size(size)
        }
    }

    #[derive(Clone, Debug, Hash, Eq, PartialEq)]
    pub struct TypeRef(libclang::TypeId);
    impl TypeRef {
        pub(crate) fn new(id: libclang::TypeId) -> Self {
            TypeRef(id)
        }
        pub fn as_cc(&self, db: &impl AstMethods) -> Outcome<cc::Ty> {
            db.type_of(self.0)
        }
        pub fn as_rs(&self, db: &impl cc::RsIr) -> Outcome<rs::Ty> {
            db.rs_type_of(self.clone())
        }
    }
}

/// C++ intermediate representation.
pub mod cc {
    use super::*;
    use crate::libclang::AstMethods;
    use std::sync::Arc;

    pub use common::{Align, Ident, Offset, Path, Size, TypeRef};

    pub trait CcIr: AstMethods {}
    impl<T> CcIr for T where T: AstMethods {}

    #[salsa::query_group(RsIrStorage)]
    #[salsa::requires(AstMethods)]
    #[salsa::requires(IrMethods)]
    pub trait RsIr {
        fn rs_bindings(&self) -> Arc<Outcome<rs::Module>>;

        fn rs_struct_from_cc(&self, id: cc::StructId) -> Outcome<rs::StructId>;

        #[salsa::dependencies]
        fn rs_type_of(&self, ty: TypeRef) -> Outcome<rs::Ty>;

        #[salsa::interned]
        fn intern_struct(&self, st: rs::Struct) -> rs::StructId;
    }

    fn rs_type_of(db: &(impl AstMethods + RsIr), ty: TypeRef) -> Outcome<rs::Ty> {
        ty.as_cc(db).then(|ty| ty.to_rust(db))
    }

    fn rs_bindings(db: &(impl AstMethods + RsIr + IrMethods)) -> Arc<Outcome<rs::Module>> {
        Arc::new(
            db.cc_ir_from_src()
                .to_ref()
                .then(|ir| ir.to_rs_bindings(db)),
        )
    }

    fn rs_struct_from_cc(db: &(impl AstMethods + RsIr), id: cc::StructId) -> Outcome<rs::StructId> {
        id.lookup(db)
            .to_rust(db, id)
            .then(|rs_st| ok(db.intern_struct(rs_st)))
    }

    intern_key!(StructId);
    impl StructId {
        pub fn lookup(&self, db: &impl AstMethods) -> Struct {
            db.lookup_intern_cc_struct(*self)
        }
    }

    intern_key!(FunctionId);
    impl FunctionId {
        pub fn lookup(&self, db: &impl AstMethods) -> Arc<Outcome<Function>> {
            db.lookup_intern_cc_fn(*self)
        }
    }

    #[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
    pub enum ItemKind {
        Struct(StructId),
    }
    impl From<StructId> for ItemKind {
        fn from(st: StructId) -> Self {
            ItemKind::Struct(st)
        }
    }

    #[derive(Clone, Debug, Eq, PartialEq, Hash)]
    #[allow(dead_code)]
    pub enum Ty {
        Error,

        Void,

        Short,
        UShort,
        Int,
        UInt,
        Long,
        ULong,
        LongLong,
        ULongLong,
        /// `char` when the default char type is signed.
        CharS,
        /// `char` when the default char type is unsigned.
        CharU,
        SChar,
        UChar,

        Size,
        SSize,
        PtrDiff,

        Float,
        Double,

        Bool,

        Struct(StructId),
    }

    #[allow(dead_code)]
    impl Ty {
        pub fn is_integral(&self) -> bool {
            use Ty::*;
            match self {
                Error => false,
                Void => false,
                Short | UShort | Int | UInt | Long | ULong | LongLong | ULongLong | CharS
                | CharU | SChar | UChar | Size | SSize | PtrDiff => true,
                Float | Double => false,
                Bool => false,
                Struct(_) => false,
            }
        }

        pub fn is_floating(&self) -> bool {
            use Ty::*;
            match self {
                Error => false,
                Void => false,
                Float | Double => true,
                Short | UShort | Int | UInt | Long | ULong | LongLong | ULongLong | CharS
                | CharU | SChar | UChar | Size | SSize | PtrDiff => false,
                Bool => false,
                Struct(_) => false,
            }
        }

        pub fn is_builtin(&self) -> bool {
            use Ty::*;
            match self {
                Error => false,
                Void => true,
                Float | Double => true,
                Short | UShort | Int | UInt | Long | ULong | LongLong | ULongLong | CharS
                | CharU | SChar | UChar | Size | SSize | PtrDiff => true,
                Bool => true,
                Struct(_) => false,
            }
        }

        pub fn is_error(&self) -> bool {
            self == &Ty::Error
        }

        pub fn is_visible(&self, db: &impl AstMethods) -> bool {
            match self {
                Ty::Struct(id) => db
                    .cc_ir_from_src()
                    .to_ref()
                    .skip_errs()
                    .exports
                    .contains(&id.clone().into()),
                _ if self.is_builtin() => true,
                _ => unreachable!(),
            }
        }

        pub fn to_rust(&self, db: &impl RsIr) -> Outcome<rs::Ty> {
            //use salsa::InternKey;
            use Ty::*;
            ok(match self {
                Error => rs::Ty::Error,
                Void => rs::Ty::Unit,
                Short => rs::Ty::I16,
                UShort => rs::Ty::U16,
                Int => rs::Ty::I32,
                UInt => rs::Ty::U32,
                Long => rs::Ty::I64, // TODO assumes LP64
                ULong => rs::Ty::U64,
                LongLong => rs::Ty::I64,
                ULongLong => rs::Ty::U64,
                CharS | SChar => rs::Ty::I8,
                CharU | UChar => rs::Ty::U8,
                Size => rs::Ty::USize,
                SSize => rs::Ty::ISize,
                PtrDiff => rs::Ty::ISize,
                Float => rs::Ty::F32,
                Double => rs::Ty::F64,
                Bool => rs::Ty::Bool,
                Struct(id) => return db.rs_struct_from_cc(*id).map(rs::Ty::Struct),
            })
        }
    }

    #[derive(Clone, Debug, Eq, PartialEq, Hash)]
    pub struct Struct {
        pub name: Path,
        pub fields: Vec<Field>,
        pub offsets: Vec<Offset>,
        pub methods: Vec<Function>,
        pub size: Size,
        pub align: Align,
        pub span: Span,
    }

    #[derive(Clone, Debug, Eq, PartialEq, Hash)]
    pub struct Field {
        pub name: Ident,
        pub ty: TypeRef,
        pub span: Span,
    }

    #[derive(Clone, Debug, Eq, PartialEq, Hash)]
    pub struct Function {
        pub name: Ident,
        pub param_tys: Vec<TypeRef>,
        pub param_names: Vec<Option<Ident>>,
        pub return_ty: TypeRef,
        /// Whether this function is a non-static method.
        ///
        /// Methods have an implicit `this` type as their first parameter.
        pub is_method: bool,
        /// For non-static methods, whether `this` is const.
        pub is_const: bool,
    }
    impl Function {
        pub fn param_tys<'a>(&'a self, db: &'a impl CcIr) -> impl Iterator<Item = Ty> + 'a {
            // skip_errs okay because errors get collected by Struct::to_rust()
            self.param_tys
                .iter()
                .map(move |ty_ref| ty_ref.as_cc(db).skip_errs())
        }
        pub fn return_ty(&self, db: &impl CcIr) -> Ty {
            self.return_ty.as_cc(db).skip_errs()
        }
    }

    impl Struct {
        pub fn to_rust(&self, db: &(impl RsIr + AstMethods), id: StructId) -> Outcome<rs::Struct> {
            let fields = self
                .fields
                .iter()
                .map(|f| {
                    f.ty.as_cc(db)
                        // Collect errors from lowering each field's type to Rust here.
                        // TODO find a more robust/explicit way.
                        .then(|cc_ty| cc_ty.to_rust(db).map(|_| cc_ty))
                        .map(|cc_ty| rs::Field {
                            name: f.name.clone(),
                            ty: f.ty.clone(),
                            span: f.span.clone(),
                            // Long term we probably don't want to condition
                            // visibility on the visibility of the type (instead
                            // controlling visibility with inner modules and `pub
                            // use`), but this works well for now.
                            vis: match cc_ty.is_visible(db) {
                                true => rs::Visibility::Public,
                                false => rs::Visibility::Private,
                            },
                        })
                })
                .collect::<Outcome<Vec<_>>>();
            let mdl = db.cc_ir_from_src();
            let mdl = mdl.to_ref().skip_errs();
            ok(())
                .then(|()| {
                    // Check method types.
                    self.methods
                        .iter()
                        .flat_map(|meth| meth.param_tys.iter().chain(Some(&meth.return_ty)))
                        .map(|ty| ty.as_rs(db).map(|_| ()))
                        .collect::<Outcome<Vec<()>>>()
                        .map(|_| ())
                })
                .then(|()| fields)
                .then(|fields| self.check_offsets(db, &fields).map(|_| fields))
                .map(|fields| rs::Struct {
                    name: self.name.clone(),
                    fields,
                    offsets: self.offsets.clone(),
                    methods: self.methods.iter().cloned().map(rs::Method).collect(),
                    vis: match mdl.exports.contains(&id.into()) {
                        true => rs::Visibility::Public,
                        false => rs::Visibility::Private,
                    },
                    repr: rs::Repr::C,
                    size: self.size,
                    align: self.align,
                    span: self.span.clone(),
                    cc_id: id,
                })
        }

        fn check_offsets(&self, db: &impl RsIr, fields: &Vec<rs::Field>) -> Outcome<()> {
            let mut offset = 0;
            let mut align = self.align;
            assert_eq!(self.fields.len(), self.offsets.len());
            for (idx, field) in fields.iter().enumerate() {
                let field_ty = field.ty(db);
                offset = common::align_to(offset, field_ty.align(db));
                align = std::cmp::max(align, field_ty.align(db));

                // Here's where we could add padding, if we wanted to.
                if offset != self.offsets[idx] {
                    return err(
                        (),
                        Diagnostic::error(
                            "unexpected field offset",
                            field
                                .span
                                .label("this field was not at the expected offset"),
                        )
                        .with_note(format!(
                            "expected an offset of {}, but the offset is {}",
                            offset, self.offsets[idx]
                        )),
                    );
                }

                offset += field_ty.size(db).0;
            }

            let size = common::align_to(offset, align);
            if size != self.size.0 || align != self.align {
                let mut diag = Diagnostic::error(
                    "unexpected struct layout",
                    self.span
                        .label("this struct does not have a standard C layout"),
                );
                if size != self.size.0 {
                    diag = diag.with_note(format!(
                        "expected a size of {}, but the size is {}",
                        size, self.size.0
                    ));
                }
                if align != self.align {
                    diag = diag.with_note(format!(
                        "expected an alignment of {}, but the alignment is {}",
                        align, self.align
                    ));
                }
                return err((), diag);
            }

            ok(())
        }
    }
}

/// Rust intermediate representation.
pub mod rs {
    use super::*;
    use cc::RsIr;

    pub use common::{Align, Ident, Offset, Path, Size, TypeRef};

    intern_key!(StructId);
    impl StructId {
        pub fn lookup(&self, db: &impl cc::RsIr) -> Struct {
            db.lookup_intern_struct(*self)
        }
    }

    #[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
    pub enum ItemKind {
        Struct(StructId),
    }

    #[derive(Debug, Clone, Eq, PartialEq)]
    pub struct Module {
        pub items: Vec<ItemKind>,
        pub exports: HashSet<ItemKind>,
    }

    impl Module {
        pub fn exported_structs<'a>(&'a self) -> impl Iterator<Item = StructId> + 'a {
            self.exports.iter().flat_map(|item| match item {
                ItemKind::Struct(id) => Some(*id),
            })
        }
    }

    /// Represents properties of a Rust type in a #[repr(C)] struct.
    #[derive(Debug, Clone, Eq, PartialEq, Hash)]
    pub enum Ty {
        Error,

        Unit,

        U8,
        I8,
        U16,
        I16,
        U32,
        I32,
        U64,
        I64,
        USize,
        ISize,
        F32,
        F64,
        Bool,

        Struct(StructId),
    }

    impl Ty {
        pub fn size(&self, db: &impl RsIr) -> Size {
            use Ty::*;
            let sz = match self {
                Error => 0,
                Unit => 0, // TODO this depends on context!
                U8 | I8 => 1,
                U16 | I16 => 2,
                U32 | I32 => 4,
                U64 | I64 => 8,
                USize => 8, // TODO make target dependent
                ISize => 8,
                F32 => 4,
                F64 => 8,
                Bool => 1,
                Struct(id) => return id.lookup(db).size,
            };
            Size::new(sz)
        }

        pub fn align(&self, db: &impl RsIr) -> Align {
            match self {
                Ty::Struct(id) => id.lookup(db).align,
                // TODO make target dependent. this assumes x86_64
                _ => Align::new(self.size(db).0),
            }
        }
    }

    #[derive(Debug, Clone, Copy, Eq, PartialEq, Hash)]
    pub enum Visibility {
        Public,
        Private,
    }

    #[derive(Debug, Clone, Eq, PartialEq, Hash)]
    pub struct Field {
        pub name: Ident,
        pub ty: TypeRef,
        pub span: Span,
        pub vis: Visibility,
    }
    impl Field {
        pub fn ty(&self, db: &impl RsIr) -> Ty {
            // skip_errs okay since we collect errors in `cc::Struct::to_rust`
            // when this Field is created.
            self.ty.as_rs(db).skip_errs()
        }
    }

    #[derive(Debug, Clone, Eq, PartialEq, Hash)]
    #[allow(dead_code)]
    pub enum Repr {
        C,
        Opaque,
    }

    #[derive(Debug, Clone, Eq, PartialEq, Hash)]
    pub struct Struct {
        pub name: Path,
        pub fields: Vec<Field>,
        pub offsets: Vec<Offset>,
        pub methods: Vec<Method>,
        pub vis: Visibility,
        pub repr: Repr,
        pub size: Size,
        pub align: Align,
        pub span: Span,
        // TODO: We might need a more general way of doing this. (Similar to TypeRef?)
        pub cc_id: cc::StructId,
    }

    #[derive(Debug, Clone, Eq, PartialEq, Hash)]
    pub struct Method(pub(super) Function);
    impl Method {
        pub fn func(&self) -> &Function {
            &self.0
        }
        pub fn param_tys<'a>(&'a self, db: &'a impl RsIr) -> impl Iterator<Item = Ty> + 'a {
            // skip_errs is okay because we check method types in Struct::to_rust above.
            self.0
                .param_tys
                .iter()
                .map(move |ty_ref| ty_ref.as_rs(db).skip_errs())
        }
        pub fn return_ty(&self, db: &impl RsIr) -> Ty {
            self.0.return_ty.as_rs(db).skip_errs()
        }
        pub fn cc_func(&self, _db: &impl RsIr) -> cc::Function {
            self.0.clone()
        }
    }

    pub use cc::{Function, FunctionId};
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Session;

    #[test]
    fn align() {
        use common::{align_to, Align};
        assert_eq!(align_to(5, Align::new(1)), 5);
        assert_eq!(align_to(2, Align::new(4)), 4);
        assert_eq!(align_to(4, Align::new(4)), 4);
        assert_eq!(align_to(0, Align::new(4)), 0);
        assert_eq!(align_to(0, Align::new(1)), 0);
    }

    #[test]
    fn pod_layout() {
        let mut sess = Session::test();
        let ir = cpp_lower!(sess, {
            struct Pod {
                int a, b;
                char c, d;
                double e, f;
            };
            namespace rust_export {
                using ::Pod;
            }
        });
        dbg!(&ir);
        let st = ir.exported_structs().next().unwrap().lookup(&sess.db);
        assert_eq!(
            st.fields
                .iter()
                .map(|f| f.name.as_str())
                .zip(st.offsets.iter().copied())
                .collect::<Vec<_>>(),
            vec![("a", 0), ("b", 4), ("c", 8), ("d", 9), ("e", 16), ("f", 24)],
        );
    }

    #[test]
    fn packed() {
        let mut sess = Session::test();
        cpp_lower!(sess, {
            struct __attribute__((__packed__)) Pod {
                int a, b;
                char c, d;
                double e, f;
            };
            namespace rust_export {
                using ::Pod;
            }
        } => [
            "packed structs not supported"
        ]);
    }

    #[test]
    fn bitfields() {
        let mut sess = Session::test();
        cpp_lower!(sess, {
            struct Pod {
                int a : 3, b : 2;
            };
            namespace rust_export {
                using ::Pod;
            }
        } => [
            "bitfields are not supported"
        ]);
    }

    #[test]
    fn nested_struct() {
        let mut sess = Session::new();
        let ir = cpp_lower!(sess, {
            struct Foo {
                int a, b;
            };
            struct Bar {
                char c, d;
                Foo foo;
            };
            namespace rust_export {
                using ::Bar;
            }
        });
        let st = ir.exported_structs().next().unwrap().lookup(&sess.db);
        assert_eq!(rs::Size::new(12), st.size);
        assert_eq!(rs::Align::new(4), st.align);
    }

    #[test]
    #[cfg_attr(windows, ignore)]  // TODO fix on other LLVM versions
    fn nested_struct_alignas() {
        let mut sess = Session::new();
        let ir = cpp_lower!(sess, {
            struct alignas(8) Foo {
                int a, b;
            };
            struct Bar {
                char c, d;
                Foo foo;
            };
            namespace rust_export {
                using ::Bar;
            }
        } => [
            "unknown attribute"  // warning
        ]);
        let st = ir.exported_structs().next().unwrap().lookup(&sess.db);
        assert_eq!(rs::Size::new(16), st.size);
        assert_eq!(rs::Align::new(8), st.align);
    }

    // TODO don't panic and report clang diagnostics
    #[test]
    #[should_panic]
    fn bad_export() {
        let mut sess = Session::test();
        cpp_lower!(sess, {
            struct Pod {
                int a;
            };
            namespace rust_export {
                using ::Missing;
            }
        } => [
            "unknown name"
        ]);
    }
}
