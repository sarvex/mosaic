#[cfg(test)]
#[macro_use]
mod test_util;

#[macro_use]
mod util;

#[macro_use]
extern crate rental;

mod codegen;
mod diagnostics;
mod index;
mod ir;
mod libclang;
mod salsa_test;

use crate::diagnostics::DiagnosticsCtx;
use salsa;
use std::env;

use libclang::File;

#[salsa::database(
    libclang::db::AstMethodsStorage,
    diagnostics::db::FileInternerStorage,
    diagnostics::db::BasicFileCacheStorage
)]
pub struct Database {
    runtime: salsa::Runtime<Database>,
}

impl salsa::Database for Database {
    fn salsa_runtime(&self) -> &salsa::Runtime<Database> {
        &self.runtime
    }
    fn salsa_runtime_mut(&mut self) -> &mut salsa::Runtime<Database> {
        &mut self.runtime
    }
}

impl Database {
    pub fn new() -> Database {
        Database {
            runtime: salsa::Runtime::default(),
        }
    }
}

pub struct Session {
    // TODO: opts
    diags: DiagnosticsCtx,
    db: Database,
}

impl Session {
    pub fn new() -> Self {
        Session {
            diags: DiagnosticsCtx::new(),
            db: Database::new(),
        }
    }

    #[cfg(test)]
    pub(crate) fn test() -> Self {
        Session {
            diags: DiagnosticsCtx::test(),
            db: Database::new(),
        }
    }
}

fn main() -> Result<(), libclang::Error> {
    use libclang::db::AstMethods;
    let mut sess = Session::new();
    let filename = env::args().nth(1).expect("Usage: cargo run <cc_file>");

    let parse = libclang::parse(&sess, &filename.into());
    sess.db.set_parse_result(parse);
    let module = &sess.db.cc_ir_from_src();
    let rs_module = module.to_ref().then(|m| m.to_rust(&sess));

    let errs = match rs_module.val() {
        Ok(rs_module) => codegen::perform_codegen(&sess, &rs_module).errs(),
        Err(errs) => errs,
    };
    errs.emit(&sess.db, &sess.diags);

    Ok(())
}
