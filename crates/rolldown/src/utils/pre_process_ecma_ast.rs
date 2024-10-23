use std::path::Path;

use itertools::Itertools;
use oxc::ast::VisitMut;
use oxc::diagnostics::Severity;
use oxc::minifier::{CompressOptions, Compressor};
use oxc::semantic::{ScopeTree, SemanticBuilder, Stats, SymbolTable};
use oxc::transformer::{
  InjectGlobalVariables, ReplaceGlobalDefines, ReplaceGlobalDefinesConfig, TransformOptions,
  Transformer,
};

use rolldown_common::NormalizedBundlerOptions;
use rolldown_ecmascript::{EcmaAst, WithMutFields};

use crate::types::oxc_parse_type::OxcParseType;

use super::ecma_visitors::EnsureSpanUniqueness;
use super::tweak_ast_for_scanning::tweak_ast_for_scanning;

#[derive(Default)]
pub struct PreProcessEcmaAst {
  /// Only recreate semantic data if ast is changed.
  ast_changed: bool,

  /// Semantic statistics.
  stats: Stats,
}

impl PreProcessEcmaAst {
  // #[allow(clippy::match_same_arms)]: `OxcParseType::Tsx` will have special logic to deal with ts compared to `OxcParseType::Jsx`
  #[allow(clippy::match_same_arms)]
  pub fn build(
    &mut self,
    mut ast: EcmaAst,
    parse_type: &OxcParseType,
    path: &Path,
    replace_global_define_config: Option<&ReplaceGlobalDefinesConfig>,
    bundle_options: &NormalizedBundlerOptions,
  ) -> anyhow::Result<(EcmaAst, SymbolTable, ScopeTree)> {
    // Build initial semantic data and check for semantic errors.
    let semantic_ret =
      ast.program.with_mut(|WithMutFields { program, .. }| SemanticBuilder::new().build(program));

    // TODO:
    // if !semantic_ret.errors.is_empty() {
    // return Err(anyhow::anyhow!("Semantic Error: {:#?}", semantic_ret.errors));
    // }

    self.stats = semantic_ret.semantic.stats();
    let (mut symbols, mut scopes) = semantic_ret.semantic.into_symbol_table_and_scope_tree();

    // Transform TypeScript and jsx.
    if !matches!(parse_type, OxcParseType::Js) {
      let ret =
        ast.program.with_mut(move |fields| {
          let mut transformer_options = TransformOptions::default();
          match parse_type {
            OxcParseType::Js => unreachable!("Should not reach here"),
            OxcParseType::Jsx | OxcParseType::Tsx => {
              transformer_options.react.jsx_plugin = true;
            }
            OxcParseType::Ts => {}
          }
          if let Some(jsx) = &bundle_options.jsx {
            transformer_options.react = jsx.clone();
          }

          Transformer::new(fields.allocator, path, transformer_options)
            .build_with_symbols_and_scopes(symbols, scopes, fields.program)
        });

      // TODO: emit diagnostic, aiming to pass more tests,
      // we ignore warning for now
      let errors = ret
        .errors
        .into_iter()
        .filter(|item| matches!(item.severity, Severity::Error))
        .collect_vec();
      if !errors.is_empty() {
        return Err(anyhow::anyhow!("Transform failed, got {:#?}", errors));
      }

      symbols = ret.symbols;
      scopes = ret.scopes;
      self.ast_changed = true;
    }

    ast.program.with_mut(|WithMutFields { allocator, program, .. }| -> anyhow::Result<()> {
      // Use built-in define plugin.
      if let Some(replace_global_define_config) = replace_global_define_config {
        let ret = ReplaceGlobalDefines::new(allocator, replace_global_define_config.clone())
          .build(symbols, scopes, program);
        symbols = ret.symbols;
        scopes = ret.scopes;
        self.ast_changed = true;
      }

      if !bundle_options.inject.is_empty() {
        let ret = InjectGlobalVariables::new(
          allocator,
          bundle_options.oxc_inject_global_variables_config.clone(),
        )
        .build(symbols, scopes, program);
        symbols = ret.symbols;
        scopes = ret.scopes;
        self.ast_changed = true;
      }

      if bundle_options.treeshake.enabled() {
        // Perform dead code elimination.
        // NOTE: `CompressOptions::dead_code_elimination` will remove `ParenthesizedExpression`s from the AST.
        let compressor = Compressor::new(allocator, CompressOptions::dead_code_elimination());
        if self.ast_changed {
          let semantic_ret = SemanticBuilder::new().with_stats(self.stats).build(program);
          (symbols, scopes) = semantic_ret.semantic.into_symbol_table_and_scope_tree();
        }
        compressor.build_with_symbols_and_scopes(symbols, scopes, program);
      }

      Ok(())
    })?;

    tweak_ast_for_scanning(&mut ast);

    ast.program.with_mut(|fields| {
      EnsureSpanUniqueness::new().visit_program(fields.program);
    });

    // NOTE: Recreate semantic data because AST is changed in the transformations above.
    (symbols, scopes) = ast.program.with_dependent(|_owner, dep| {
      SemanticBuilder::new()
        // Required by `module.scope.get_child_ids` in `crates/rolldown/src/utils/renamer.rs`.
        .with_scope_tree_child_ids(true)
        // Preallocate memory for the underlying data structures.
        .with_stats(self.stats)
        .build(&dep.program)
        .semantic
        .into_symbol_table_and_scope_tree()
    });

    Ok((ast, symbols, scopes))
  }
}
