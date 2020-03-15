use crate::ir::cc::RsIr;
use crate::{ir, libclang, Session};
use clang::{self, TranslationUnit, Unsaved};
use lazy_static::lazy_static;
use std::path::Path;
use std::sync::Arc;

lazy_static! {
    pub(crate) static ref CLANG: Arc<clang::Clang> = Arc::new(clang::Clang::new().unwrap());
}

macro_rules! cpp_parse {
    { $clang:expr, $src:tt } => { $crate::test_util::parse($clang, stringify!($src)) }
}

macro_rules! cpp_lower {
    { $sess:expr, $src:tt => [ $( $errs:expr ),* ] } => {
        $crate::test_util::parse_and_lower(&mut $sess, stringify!($src), vec![$($errs),*])
    };
    { $sess:expr, $src:tt } => {
        $crate::test_util::parse_and_lower(&mut $sess, stringify!($src), vec![])
    };
}

pub(crate) fn parse<'c>(index: &'c clang::Index, src: &str) -> TranslationUnit<'c> {
    assert!(src.starts_with('{'));
    assert!(src.ends_with('}'));
    let src = &src[1..src.len() - 1];
    let test_filename = Path::new("__test__/test.cc");
    let mut parser = libclang::configure(index.parser(&test_filename));
    let unsaved = Unsaved::new(&test_filename, src);
    parser
        .unsaved(&[unsaved])
        .parse()
        .expect("test input failed to parse")
}

pub(crate) fn parse_and_lower(
    sess: &mut Session,
    src: &str,
    expected: Vec<&str>,
) -> ir::rs::Module {
    assert!(!sess.diags.has_errors()); // TODO has_diags()

    let diags = &sess.diags;

    let ast = libclang::parse_with(CLANG.clone(), &sess, |index| parse(index, src));
    let (rust_ir, errs) = libclang::set_ast(&mut sess.db, ast, |db| {
        let rs_ir = db.rs_ir();
        let (mdl, errs) = rs_ir.to_ref().split();
        errs.clone().emit(db, diags);
        (mdl.clone(), errs.clone())
    });
    assert_eq!(
        expected,
        errs.iter().map(|diag| diag.message()).collect::<Vec<_>>(),
        "did not get the expected set of lowering errors"
    );
    rust_ir.clone()
}
