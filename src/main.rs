#[cfg(test)]
#[macro_use]
mod test_util;

#[macro_use]
mod util;

mod codegen;
mod index;
mod ir;

use crate::index::{Ident, Path};
use crate::ir::*;
use crate::util::DisplayName;
use clang::{
    self, Accessibility, Clang, Entity, EntityKind, Parser, SourceError, TranslationUnit, Type,
    TypeKind,
};
use std::convert::TryInto;
use std::env;

fn main() -> Result<(), SourceError> {
    let clang = Clang::new().unwrap();
    let index = clang::Index::new(&clang, false, true);
    let filename = env::args().nth(1).expect("Usage: cargo run <cc_file>");
    let tu = configure(index.parser(filename)).parse()?;
    BindGen::new(tu).gen()?;
    Ok(())
}

pub(crate) fn configure(mut parser: Parser<'_>) -> Parser<'_> {
    parser.skip_function_bodies(true).arguments(&[
        "-std=c++17",
        "-isysroot",
        "/Library/Developer/CommandLineTools/SDKs/MacOSX.sdk",
    ]);
    parser
}

enum Export<'tu> {
    Decl(Entity<'tu>),
    Type(Type<'tu>),
    TemplateType(Entity<'tu>),
}

struct BindGen<'tu> {
    tu: TranslationUnit<'tu>,
    //visible: Vec<(Path, Entity<'tu>)>,
}

impl<'tu> BindGen<'tu> {
    fn new(tu: TranslationUnit<'tu>) -> Self {
        BindGen {
            tu,
            //visible: vec![],
        }
    }

    fn gen(&mut self) -> Result<&mut Self, SourceError> {
        //let mut visitor = Visitor::new(&self);
        let mut exports = vec![];
        for ent in self.tu.get_entity().get_children() {
            if let EntityKind::Namespace = ent.get_kind() {
                if let Some("rust_export") = ent.get_name().as_deref() {
                    self.handle_rust_export(ent, &mut exports);
                }
            }
        }

        let mut mdl = ir::Module::new();
        for (name, export) in exports {
            match export {
                Export::Decl(decl_ref) => self.lower_decl(name, decl_ref, &mut mdl),
                Export::Type(ty) => {
                    println!("{} = {:?}", name, ty);
                    println!(
                        "  {:?}",
                        ty.get_elaborated_type()
                            .unwrap() // TODO hack
                            .get_template_argument_types()
                    );
                }
                Export::TemplateType(t) => {
                    println!("{} = {:?}", name, t);
                    for child in t.get_children() {
                        match child.get_kind() {
                            EntityKind::TemplateTypeParameter => {
                                println!("  type parameter {}", child.get_name().unwrap())
                            }
                            EntityKind::TypeAliasDecl => println!(
                                "  type alias => {:?} => {:?}",
                                child.get_typedef_underlying_type().unwrap(),
                                child
                                    .get_typedef_underlying_type()
                                    .unwrap()
                                    .get_declaration(),
                            ),
                            _ => println!("  unknown child {:?}", child),
                        }
                    }
                }
            }
        }

        mdl.check();
        codegen::perform_codegen(&mdl);

        Ok(self)
    }

    fn handle_rust_export(&self, ns: Entity<'tu>, exports: &mut Vec<(Path, Export<'tu>)>) {
        for decl in ns.get_children() {
            println!("{:?}", decl);
            let name = Path::from(decl.get_name().unwrap());
            match decl.get_kind() {
                EntityKind::UsingDeclaration => {
                    exports.push((name, Export::Decl(decl.get_reference().unwrap())))
                }
                EntityKind::TypeAliasDecl => exports.push((
                    name,
                    Export::Type(decl.get_typedef_underlying_type().unwrap()),
                )),
                EntityKind::TypeAliasTemplateDecl => {
                    exports.push((name, Export::TemplateType(decl)))
                }
                _ => panic!(
                    "Only using declarations are permitted inside rust_export:\n{}",
                    decl.get_pretty_printer().print()
                ),
            }
        }
    }

    fn lower_decl(&self, name: Path, decl_ref: Entity<'tu>, mdl: &mut ir::Module) {
        let overloads = decl_ref.get_overloaded_declarations().unwrap();
        assert_eq!(overloads.len(), 1);
        let ent = overloads[0];

        println!("{} = {:?}", name, ent);
        for child in ent.get_children() {
            println!("  {}: {:?}", child.display_name(), child.get_kind());
        }

        match ent.get_kind() {
            EntityKind::StructDecl => self.lower_struct(name, ent, mdl),
            other => eprintln!("{}: Unsupported type {:?}", name, other),
        }
    }

    fn lower_struct(&self, name: Path, ent: Entity<'tu>, mdl: &mut ir::Module) {
        let ty = ent.get_type().unwrap();
        if !ty.is_pod() {
            // TODO: Proper error handling
            eprintln!("{}: Only POD structs are supported", name);
            return;
        }

        let mut fields = vec![];
        let mut offsets = vec![];

        // TODO get rid of unwrap()
        for field in ty.get_fields().unwrap() {
            if let Some(acc) = field.get_accessibility() {
                if Accessibility::Public != acc {
                    continue;
                }
            }
            let field_name = field.get_name().unwrap();
            let field_ty = field.get_type().unwrap();
            fields.push(Field {
                name: Ident::from(field_name),
                ty: field_ty.lower(),
            });
            let offset = field.get_offset_of_field().unwrap().try_into().unwrap();
            offsets.push(offset);
        }
        let size = ty.get_alignof().unwrap().try_into().unwrap();
        let align = ty.get_alignof().unwrap().try_into().unwrap();

        let lowered = ir::Struct {
            name: name.clone(),
            fields,
            offsets,
            repr: ir::Repr::C,
            size: ir::Size::new(size).unwrap(),
            align: ir::Align::new(align).unwrap(),
        };
        mdl.structs.push(lowered);
    }
}

trait Lower {
    type Output;
    fn lower(&self) -> Self::Output;
}

impl<'tu> Lower for Type<'tu> {
    type Output = Ty;
    fn lower(&self) -> Ty {
        use TypeKind::*;
        match self.get_kind() {
            Int => Ty::Int,
            UInt => Ty::UInt,
            CharS => Ty::CharS,
            SChar => Ty::SChar,
            CharU => Ty::CharU,
            UChar => Ty::UChar,
            Float => Ty::Float,
            Double => Ty::Double,
            _ => panic!("unsupported type {:?}", self),
        }
    }
}

#[allow(dead_code)]
struct Visitor<'cpp> {
    namespace: String,
    current_ty: Option<Type<'cpp>>,
    indent: usize,
}

#[allow(dead_code)]
impl<'cpp> Visitor<'cpp> {
    fn new() -> Visitor<'cpp> {
        Visitor {
            namespace: String::new(),
            current_ty: None,
            indent: 0,
        }
    }

    fn visit_children(&mut self, ent: Entity<'cpp>) {
        let entities = ent.get_children().into_iter();
        for ent in entities {
            use EntityKind::*;
            match ent.get_kind() {
                Namespace => self.handle_namespace(ent),
                StructDecl | ClassDecl => self.handle_struct(ent),
                FieldDecl => self.handle_field(ent),
                Method => self.handle_method(ent),
                TypedefDecl => self.handle_typedef(ent),
                ClassTemplate => self.handle_class_template(ent),
                _ => (),
            }
        }
    }

    fn handle_namespace(&mut self, ns: Entity<'cpp>) {
        let full = format!("{}::{}", self.namespace, ns.display_name());
        let orig = std::mem::replace(&mut self.namespace, full);
        self.visit_children(ns);
        self.namespace = orig;
    }

    fn handle_struct(&mut self, st: Entity<'cpp>) {
        print_indent!(self, "struct {}::{}", self.namespace, st.display_name());
        let ty = st.get_type().unwrap();
        match ty.get_sizeof() {
            Ok(size) => println!(" ({} bytes)", size),
            Err(e) => println!(" ({})", e),
        }

        self.indent += 1;
        self.visit_with_ty(Some(ty), st);
        self.indent -= 1;
    }

    fn handle_field(&mut self, field: Entity<'cpp>) {
        match field.get_name() {
            Some(name) => {
                print_indent!(self, "{}", field.display_name());
                if let Some(ty) = field.get_type() {
                    print!(": {}", ty.display_name());
                }
                if let Some(struct_ty) = self.current_ty {
                    match struct_ty.get_offsetof(&name) {
                        Ok(offset) => print!(" (offset {} bits)", offset),
                        Err(e) => print!(" ({})", e),
                    }
                }
                println!();
            }
            None => {
                println!("field: {}", field.display_name());
            }
        }
    }

    fn handle_method(&mut self, meth: Entity<'cpp>) {
        print_indent!(self, "{}", meth.display_name());
        if let Some(ty) = meth.get_result_type() {
            print!(" -> {}", ty.display_name());
        }
        println!();
    }

    fn handle_typedef(&mut self, td: Entity<'cpp>) {
        println_indent!(
            self,
            "typedef {} = {}",
            td.display_name(),
            td.get_typedef_underlying_type().unwrap().display_name()
        );
    }

    fn handle_class_template(&mut self, t: Entity<'cpp>) {
        println_indent!(self, "template {}::{}", self.namespace, t.display_name());
        let name = format!(
            "{}::{}",
            self.namespace,
            t.get_name().unwrap_or("?".to_string())
        );
        println_indent!(self, "  => {}", name);

        self.indent += 1;
        self.visit_with_ty(None, t);
        self.indent -= 1;
    }

    fn visit_with_ty(&mut self, ty: Option<Type<'cpp>>, ent: Entity<'cpp>) {
        let orig = std::mem::replace(&mut self.current_ty, ty);
        self.visit_children(ent);
        self.current_ty = orig;
    }
}
