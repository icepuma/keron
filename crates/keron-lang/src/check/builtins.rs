//! Built-in function signatures seeded into the checker's `fn_env`.
//!
//! Each builtin returns a resource-typed value. Resources are pure
//! values: they don't touch disk until promoted via `realize`. The
//! lowercase fn names reuse the regular call infrastructure
//! (positional/named args, type checking) — keron treats them as
//! ordinary functions whose definitions just happen to live outside
//! the source program.

use std::collections::HashMap;

use crate::ast::Type;

use super::{FnEnv, FnSig, ParamSig};

pub(super) fn builtin_fn_env() -> FnEnv {
    let mut env: FnEnv = HashMap::new();
    env.insert(
        "symlink".into(),
        FnSig {
            params: vec![
                ParamSig {
                    name: "from".into(),
                    ty: Type::String,
                    has_default: false,
                },
                ParamSig {
                    name: "to".into(),
                    ty: Type::String,
                    has_default: false,
                },
            ],
            return_type: Type::Symlink,
        },
    );
    env.insert(
        "file".into(),
        FnSig {
            params: vec![
                ParamSig {
                    name: "path".into(),
                    ty: Type::String,
                    has_default: false,
                },
                ParamSig {
                    name: "content".into(),
                    ty: Type::String,
                    has_default: false,
                },
            ],
            return_type: Type::File,
        },
    );
    env.insert(
        "directory".into(),
        FnSig {
            params: vec![ParamSig {
                name: "path".into(),
                ty: Type::String,
                has_default: false,
            }],
            return_type: Type::Directory,
        },
    );
    env
}
