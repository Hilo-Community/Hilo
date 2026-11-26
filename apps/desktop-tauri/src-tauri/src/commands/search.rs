//! Workspace search (FTS5 + unsaved buffers) and AST / impacted-test analysis.

use context_engine::{GraphEdgeConfidence, SymbolKind};
use host_core::ExcerptFreshness;
use serde::Serialize;
use tauri::Manager;

use crate::state::DesktopAppState;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DesktopSearchResult {
    file_path: String,
    line: u32,
    snippet: String,
    freshness: &'static str,
    score: f64,
    secrets_redacted: usize,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DesktopAstSymbol {
    name: String,
    kind: &'static str,
    line: u32,
    exported: bool,
    confidence: &'static str,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DesktopImpactedTest {
    test_file_path: String,
    covered_symbol: String,
    confidence: &'static str,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DesktopAstAnalysis {
    symbols: Vec<DesktopAstSymbol>,
    impacted_tests: Vec<DesktopImpactedTest>,
}

/// Runs on a blocking worker: a search must never stall the window.
#[tauri::command]
pub async fn search_workspace_command(
    app: tauri::AppHandle,
    query: String,
    limit: usize,
) -> Result<Vec<DesktopSearchResult>, String> {
    tauri::async_runtime::spawn_blocking(move || {
        search_workspace(&app.state::<DesktopAppState>(), &query, limit)
    })
    .await
    .map_err(|error| format!("Search task failed: {error}"))?
}

fn search_workspace(
    state: &DesktopAppState,
    query: &str,
    limit: usize,
) -> Result<Vec<DesktopSearchResult>, String> {
    let root = state
        .workspace_root
        .lock()
        .map_err(|_| "workspace root lock poisoned".to_string())?
        .clone();
    // Lock order: bridge, then context engine. The documents are only needed to refresh the
    // unsaved-buffer overlay; the editor gets the bridge back before the query runs.
    let bridge = state
        .bridge
        .lock()
        .map_err(|_| "desktop document bridge lock poisoned".to_string())?;
    let mut context_guard = state
        .context_engine
        .lock()
        .map_err(|_| "context engine lock poisoned".to_string())?;
    let context_engine = context_guard
        .as_mut()
        .ok_or_else(|| "Workspace search is not initialized".to_string())?;
    context_engine
        .overlay_mut()
        .sync_from_document_service(root.as_deref(), bridge.document_service());
    drop(bridge);
    let hits = context_engine.query_hybrid(query, limit.min(200));
    Ok(hits
        .into_iter()
        .map(|hit| {
            let freshness = match hit.excerpt.freshness {
                ExcerptFreshness::LiveUnsavedBuffer => "LiveUnsavedBuffer",
                ExcerptFreshness::IndexedCommit => "IndexedCommit",
                ExcerptFreshness::StaleNeedsReindex => "StaleNeedsReindex",
            };
            let secrets_redacted = hit.excerpt.content.matches("[REDACTED").count();
            DesktopSearchResult {
                file_path: hit.excerpt.source_path,
                line: hit.excerpt.start_line_col.line.saturating_add(1),
                snippet: hit.excerpt.content,
                freshness,
                score: hit.combined_score,
                secrets_redacted,
            }
        })
        .collect())
}

/// Asked after every acknowledged edit, so it runs on a blocking worker: parsing the whole
/// document on the main thread would stall the window while the user types.
#[tauri::command]
pub async fn analyze_document_command(
    app: tauri::AppHandle,
    document_id: String,
    session_generation: u32,
) -> Result<DesktopAstAnalysis, String> {
    tauri::async_runtime::spawn_blocking(move || {
        analyze_document(&app, &document_id, session_generation)
    })
    .await
    .map_err(|error| format!("Analysis task failed: {error}"))?
}

fn analyze_document(
    app: &tauri::AppHandle,
    document_id: &str,
    session_generation: u32,
) -> Result<DesktopAstAnalysis, String> {
    let state = app.state::<DesktopAppState>();
    // Confirming import edges asks vtsls, which can take a while. The next analysis shows the
    // result.
    {
        let app = app.clone();
        let document_id = document_id.to_owned();
        tauri::async_runtime::spawn_blocking(move || {
            crate::commands::semantic::enrich_import_edges(
                &app.state::<DesktopAppState>(),
                &document_id,
                session_generation,
            )
        });
    }
    // Only the (O(1)) rope copy happens under the bridge lock: the parse does not hold up edits.
    let rope = state
        .bridge
        .lock()
        .map_err(|_| "desktop document bridge lock poisoned".to_string())?
        .document_rope(document_id, session_generation)
        .ok_or_else(|| "The active document session is unavailable".to_string())?;
    let symbols = crate::bridge::document_symbols(document_id, &rope);
    let symbol_pairs: Vec<(&str, &str)> = symbols
        .iter()
        .map(|symbol| (document_id, symbol.name.as_str()))
        .collect();
    let impacted_tests = state
        .context_engine
        .lock()
        .map_err(|_| "context engine lock poisoned".to_string())?
        .as_ref()
        .map(|engine| {
            engine
                .impact_analyzer()
                .find_impacted_tests(&symbol_pairs, 4)
        })
        .unwrap_or_default();
    let symbols = symbols
        .into_iter()
        .map(|symbol| DesktopAstSymbol {
            name: symbol.name,
            kind: match symbol.kind {
                SymbolKind::Function => "function",
                SymbolKind::ReactComponent => "component",
                SymbolKind::Class => "class",
                SymbolKind::Interface => "interface",
                SymbolKind::TypeAlias | SymbolKind::JsonKey => "type",
                SymbolKind::ConstOrVariable | SymbolKind::CssSelector => "const",
            },
            line: symbol.start_point.row.saturating_add(1),
            exported: symbol.exported,
            confidence: "ExactAstLsp",
        })
        .collect();
    let impacted_tests = impacted_tests
        .into_iter()
        .flat_map(|test| {
            let confidence = match test.confidence {
                GraphEdgeConfidence::ExactSemantic | GraphEdgeConfidence::SyntacticAst => {
                    "ExactAstLsp"
                }
                GraphEdgeConfidence::HeuristicText => "HeuristicText",
            };
            test.matched_symbols
                .into_iter()
                .map(move |covered_symbol| DesktopImpactedTest {
                    test_file_path: test.test_file_path.clone(),
                    covered_symbol,
                    confidence,
                })
        })
        .collect();
    Ok(DesktopAstAnalysis {
        symbols,
        impacted_tests,
    })
}
