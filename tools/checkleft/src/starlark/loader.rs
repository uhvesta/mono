use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use starlark::environment::{FrozenModule, Globals, Module};
use starlark::eval::{Evaluator, FileLoader};
use starlark::syntax::{AstModule, Dialect, DialectTypes};
use starlark::values::FrozenHeapName;

use crate::input::SourceTree;
use crate::path::validate_relative_path;

#[derive(Debug, Clone)]
pub(crate) struct LoadContext {
    pub checkleft_root: PathBuf,
    pub check_dir: PathBuf,
}

pub(crate) struct CheckleftFileLoader<'a> {
    pub tree: &'a dyn SourceTree,
    pub globals: &'a Globals,
    pub context: LoadContext,
}

impl FileLoader for CheckleftFileLoader<'_> {
    fn load(&self, path: &str) -> starlark::Result<FrozenModule> {
        self.load_module(path).map_err(starlark::Error::new_other)
    }
}

impl CheckleftFileLoader<'_> {
    fn load_module(&self, module_id: &str) -> Result<FrozenModule> {
        let path = resolve_load_path(&self.context, module_id)?;
        let source = String::from_utf8(
            self.tree
                .read_file(&path)
                .with_context(|| format!("failed to read loaded module {}", path.display()))?,
        )
        .with_context(|| format!("{} is not valid UTF-8", path.display()))?;
        let ast = AstModule::parse(path.to_string_lossy().as_ref(), source, &load_dialect())
            .map_err(|e| anyhow!(e))
            .with_context(|| format!("failed to parse loaded module {}", path.display()))?;

        Module::with_temp_heap(|module| {
            {
                let mut eval = Evaluator::new(&module);
                eval.set_loader(self);
                eval.eval_module(ast, self.globals)?;
            }
            Ok(module.freeze_named(FrozenHeapName::User(Box::new(path.to_string_lossy().to_string())))?)
        })
        .map_err(|e: starlark::Error| anyhow!(e.to_string()))
    }
}

fn resolve_load_path(context: &LoadContext, module_id: &str) -> Result<PathBuf> {
    if module_id.starts_with('@') {
        bail!("external Starlark package loads are not supported yet: {module_id}");
    }

    if let Some(lib_path) = module_id.strip_prefix("//lib/") {
        return resolve_checkleft_module(&context.checkleft_root.join("lib"), lib_path, module_id);
    }

    if let Some(local_path) = module_id.strip_prefix(':') {
        return resolve_checkleft_module(&context.check_dir, local_path, module_id);
    }

    bail!("unsupported Starlark load path `{module_id}`; use //lib/name or :name");
}

fn resolve_checkleft_module(base: &Path, module_path: &str, module_id: &str) -> Result<PathBuf> {
    if module_path.trim().is_empty() {
        bail!("Starlark load path `{module_id}` must name a module");
    }
    let relative = Path::new(module_path);
    validate_relative_path(relative).with_context(|| format!("invalid Starlark load path `{module_id}`"))?;
    if relative.extension().is_some() {
        bail!("Starlark load path `{module_id}` must omit the .checkleft extension");
    }
    Ok(base.join(relative).with_extension("checkleft"))
}

fn load_dialect() -> Dialect {
    Dialect {
        enable_types: DialectTypes::Enable,
        enable_load: true,
        enable_keyword_only_arguments: true,
        enable_f_strings: true,
        ..Dialect::Standard
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn context() -> LoadContext {
        LoadContext {
            checkleft_root: PathBuf::from("checkleft"),
            check_dir: PathBuf::from("checkleft/text/no_debug"),
        }
    }

    #[test]
    fn resolves_lib_load_under_checkleft_lib() {
        assert_eq!(
            resolve_load_path(&context(), "//lib/messages").expect("resolve"),
            PathBuf::from("checkleft/lib/messages.checkleft")
        );
    }

    #[test]
    fn resolves_colon_load_under_check_directory() {
        assert_eq!(
            resolve_load_path(&context(), ":predicates").expect("resolve"),
            PathBuf::from("checkleft/text/no_debug/predicates.checkleft")
        );
    }

    #[test]
    fn rejects_external_package_loads() {
        let err = resolve_load_path(&context(), "@dep//lib/messages").expect_err("external load");

        assert!(
            err.to_string()
                .contains("external Starlark package loads are not supported")
        );
    }

    #[test]
    fn rejects_path_traversal() {
        let err = resolve_load_path(&context(), ":../secrets").expect_err("traversal");

        assert!(err.to_string().contains("invalid Starlark load path"));
    }
}
