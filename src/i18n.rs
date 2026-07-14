//! Lightweight i18n for ukmm-extractool.
//!
//! Two built-in languages (English, French). Detection order:
//! 1. `UKMM_LANG` environment variable (`en` / `fr`)
//! 2. System locale via `sys-locale`
//! 3. English (fallback)

use std::collections::HashMap;

/// Language context holding all translated strings.
pub struct Lang {
    /// `true` for English, `false` for French.
    #[allow(dead_code)]
    pub is_en: bool,
    strings: HashMap<&'static str, &'static str>,
}

impl Lang {
    /// Detect language and build the string table.
    pub fn detect() -> Self {
        let lang = std::env::var("UKMM_LANG")
            .ok()
            .or_else(sys_locale::get_locale);
        let is_en = !lang
            .as_deref()
            .map(|l| l.starts_with("fr"))
            .unwrap_or(false);
        let strings = if is_en { build_en() } else { build_fr() };
        Self { is_en, strings }
    }

    /// Force a specific language (`"en"` or `"fr"`).
    pub fn force(code: &str) -> Self {
        let is_en = code != "fr";
        let strings = if is_en { build_en() } else { build_fr() };
        Self { is_en, strings }
    }

    /// Look up a translated string by key.
    ///
    /// Returns the key itself if not found (graceful fallback).
    pub fn t(&self, key: &'static str) -> &str {
        self.strings.get(key).copied().unwrap_or(key)
    }
}

// ── English ──────────────────────────────────────────────────────────────────

fn build_en() -> HashMap<&'static str, &'static str> {
    let mut m = HashMap::new();

    // App identity
    m.insert("app.title", "ukmm-extractool");
    m.insert("app.subtitle", "Extract & rebuild UKMM mod files");
    m.insert("app.version", env!("CARGO_PKG_VERSION"));

    // Menu
    m.insert("menu.prompt", "What would you like to do?");
    m.insert("menu.extract", "Extract a mod");
    m.insert("menu.rebuild", "Rebuild a mod");
    m.insert("menu.restore", "Restore a mod");
    m.insert("menu.list", "List available mods");
    m.insert("menu.info", "Information");
    m.insert("menu.quit", "Quit");

    // Common
    m.insert("common.yes", "Yes");
    m.insert("common.no", "No");
    m.insert("common.cancel", "Cancel");
    m.insert("common.confirm", "Are you sure?");
    m.insert("common.proceed", "Proceed");
    m.insert("common.done", "Done!");
    m.insert("common.error", "Error");
    m.insert("common.warning", "Warning");
    m.insert("common.skipped", "Skipped");
    m.insert("common.cancelled", "Cancelled");
    m.insert("common.press_enter", "Press Enter to continue...");
    m.insert("common.back_to_menu", "Press Enter to return to menu...");
    m.insert("common.file_not_found", "File not found: {path}");
    m.insert("common.unsupported_ext", "Unsupported file extension: {ext}");
    m.insert("common.return_to_menu", "Return to menu.");
    m.insert("common.platform_wiiu", "Wii U");
    m.insert("common.platform_switch", "Switch");
    m.insert("common.output_dir", "Output");
    m.insert("common.files_count", "Files");

    // Format names (used in log output, may be shown to users)
    m.insert("fmt.byml", "BYML");
    m.insert("fmt.aamp", "AAMP");
    m.insert("fmt.actorinfo", "ActorInfo");
    m.insert("fmt.message", "Message");
    m.insert("fmt.mergeable", "Mergeable");
    m.insert("fmt.sarcmap", "SarcMap");
    m.insert("fmt.binary", "Binary");
    m.insert("fmt.sarc", "SARC");
    m.insert("fmt.cbor", "CBOR");

    // Extract flow
    m.insert("extract.title", "Extract a Mod");
    m.insert("extract.scanning", "Scanning UKMM mods...");
    m.insert("extract.no_mods", "No UKMM mods found.");
    m.insert("extract.no_mods_hint", "Make sure UKMM is installed and has mods in {path}");
    m.insert("extract.select_prompt", "Select a mod to extract:");
    m.insert("extract.loading_zip", "Reading ZIP contents...");
    m.insert("extract.progress", "Converting [{current}/{total}] {file}...");
    m.insert("extract.converted_byml", "Converted to native BYML: {path}");
    m.insert("extract.backup_saved", "Backup saved: {path}");
    m.insert("extract.summary_title", "─── Extract Summary ───");
    m.insert("extract.summary_mod", "Mod");
    m.insert("extract.summary_platform", "Platform");
    m.insert("extract.summary_converted", "Converted");
    m.insert("extract.summary_skipped", "Skipped");
    m.insert("extract.summary_output", "Output");
    m.insert("extract.no_files", "No UKMM resource files found in the mod.");
    m.insert("extract.extracting_zip", "Extracting ZIP...");
    m.insert("extract.copying_dir", "Copying loose mod folder...");

    // Rebuild flow
    m.insert("rebuild.title", "Rebuild a Mod");
    m.insert("rebuild.scanning", "Scanning workspaces...");
    m.insert("rebuild.select_prompt", "Select a workspace to rebuild:");
    m.insert("rebuild.no_workspaces", "No workspaces found in {path}");
    m.insert("rebuild.confirm", "Rebuild '{mod_name}' and copy to UKMM?");
    m.insert("rebuild.confirm_details", "This will overwrite the mod in UKMM with your edited files.");
    m.insert("rebuild.progress", "Rebuilding [{current}/{total}] {file}...");
    m.insert("rebuild.converting", "Converting ({fmt}): {src} → {dst}");
    m.insert("rebuild.adding_zip", "Added to ZIP: {name}");
    m.insert("rebuild.copied_ukmm", "Copied to UKMM: {path}");
    m.insert("rebuild.done", "Rebuilt {n} file(s). Output: {path}");
    m.insert("rebuild.no_files", "No edited files found in workspace.");
    m.insert("rebuild.dup_found", "Both .yaml and .sbyml exist for '{stem}'. Which should be used?");
    m.insert("rebuild.dup_sbyml", "Use .sbyml (native BYML)");
    m.insert("rebuild.dup_yaml", "Use .yaml (editable YAML)");

    // Restore flow
    m.insert("restore.title", "Restore a Mod");
    m.insert("restore.select_prompt", "Select a workspace to restore:");
    m.insert("restore.confirm", "Restore '{mod_name}' from backup?");
    m.insert("restore.confirm_details", "This will overwrite the mod in UKMM with the original backup.");
    m.insert("restore.done", "Restored {mod_name} to original.");
    m.insert("restore.no_backup", "No backup found for this workspace.");

    // Inspect & Convert (removed from CLI, strings kept for legacy message references)
    // List mods
    m.insert("list.title", "Available Mods");
    m.insert("list.none", "No mods found.");
    m.insert("list.header", "{n} mod(s) found in UKMM:\n");

    // Info screen
    m.insert("info.title", "ukmm-extractool — Information");
    m.insert("info.desc", "Extract and rebuild UKMM mod files to/from editable YAML and native BYML.");
    m.insert("info.formats", "Supported formats: .byml / .sbyml / .sarc / .ssarc / .bshop / .aamp / .sbshop / .bdemo / .bfarc / .pack");
    m.insert("info.pipeline", "Pipeline: file → decompress → detect format → parse → serialize YAML / BYML");
    m.insert("info.rebuild", "Rebuild: YAML / .sbyml → CBOR wire format → zstd compress → inject into ZIP");

    // Errors
    m.insert("err.generic", "{msg}");
    m.insert("err.invalid_selection", "Invalid selection.");
    m.insert("err.no_valid_files", "No valid files could be processed.");
    m.insert("err.rebuild_failed", "Rebuild failed: {msg}");
    m.insert("err.extract_failed", "Extraction failed: {msg}");

    m
}

// ── French ───────────────────────────────────────────────────────────────────

fn build_fr() -> HashMap<&'static str, &'static str> {
    let mut m = HashMap::new();

    m.insert("app.title", "ukmm-extractool");
    m.insert("app.subtitle", "Extraire & reconstruire les mods UKMM");
    m.insert("app.version", env!("CARGO_PKG_VERSION"));

    m.insert("menu.prompt", "Que souhaitez-vous faire ?");
    m.insert("menu.extract", "Extraire un mod");
    m.insert("menu.rebuild", "Reconstruire un mod");
    m.insert("menu.restore", "Restaurer un mod");
    m.insert("menu.inspect", "Inspecter un fichier");
    m.insert("menu.convert", "Convertir un fichier seul");
    m.insert("menu.list", "Lister les mods disponibles");
    m.insert("menu.info", "Informations");
    m.insert("menu.quit", "Quitter");

    m.insert("common.yes", "Oui");
    m.insert("common.no", "Non");
    m.insert("common.cancel", "Annuler");
    m.insert("common.confirm", "Êtes-vous sûr ?");
    m.insert("common.proceed", "Continuer");
    m.insert("common.done", "Terminé !");
    m.insert("common.error", "Erreur");
    m.insert("common.warning", "Attention");
    m.insert("common.skipped", "Ignoré");
    m.insert("common.cancelled", "Annulé");
    m.insert("common.press_enter", "Appuyez sur Entrée pour continuer...");
    m.insert("common.back_to_menu", "Appuyez sur Entrée pour revenir au menu...");
    m.insert("common.file_not_found", "Fichier introuvable : {path}");
    m.insert("common.unsupported_ext", "Extension de fichier non prise en charge : {ext}");
    m.insert("common.return_to_menu", "Retour au menu.");
    m.insert("common.platform_wiiu", "Wii U");
    m.insert("common.platform_switch", "Switch");
    m.insert("common.output_dir", "Sortie");
    m.insert("common.files_count", "Fichiers");

    m.insert("fmt.byml", "BYML");
    m.insert("fmt.aamp", "AAMP");
    m.insert("fmt.actorinfo", "ActorInfo");
    m.insert("fmt.message", "Message");
    m.insert("fmt.mergeable", "Mergeable");
    m.insert("fmt.sarcmap", "SarcMap");
    m.insert("fmt.binary", "Binary");
    m.insert("fmt.sarc", "SARC");
    m.insert("fmt.cbor", "CBOR");

    m.insert("extract.title", "Extraire un mod");
    m.insert("extract.scanning", "Recherche des mods UKMM...");
    m.insert("extract.no_mods", "Aucun mod UKMM trouvé.");
    m.insert("extract.no_mods_hint", "Assurez-vous que UKMM est installé et contient des mods dans {path}");
    m.insert("extract.select_prompt", "Choisissez un mod à extraire :");
    m.insert("extract.loading_zip", "Lecture du contenu du ZIP...");
    m.insert("extract.progress", "Conversion [{current}/{total}] {file}...");
    m.insert("extract.converted_byml", "Converti en BYML natif : {path}");
    m.insert("extract.backup_saved", "Sauvegarde créée : {path}");
    m.insert("extract.summary_title", "Résumé de l'extraction");
    m.insert("extract.summary_mod", "Mod");
    m.insert("extract.summary_platform", "Plateforme");
    m.insert("extract.summary_converted", "Convertis");
    m.insert("extract.summary_skipped", "Ignorés");
    m.insert("extract.summary_output", "Sortie");
    m.insert("extract.no_files", "Aucun fichier ressource UKMM trouvé dans le mod.");
    m.insert("extract.extracting_zip", "Extraction du ZIP...");
    m.insert("extract.copying_dir", "Copie du dossier du mod...");

    m.insert("rebuild.title", "Reconstruire un mod");
    m.insert("rebuild.scanning", "Recherche des espaces de travail...");
    m.insert("rebuild.select_prompt", "Choisissez un espace de travail à reconstruire :");
    m.insert("rebuild.no_workspaces", "Aucun espace de travail trouvé dans {path}");
    m.insert("rebuild.confirm", "Reconstruire '{mod_name}' et le copier vers UKMM ?");
    m.insert("rebuild.confirm_details", "Cela écrasera le mod dans UKMM avec vos fichiers modifiés.");
    m.insert("rebuild.progress", "Reconstruction [{current}/{total}] {file}...");
    m.insert("rebuild.converting", "Conversion ({fmt}) : {src} → {dst}");
    m.insert("rebuild.adding_zip", "Ajouté au ZIP : {name}");
    m.insert("rebuild.copied_ukmm", "Copié vers UKMM : {path}");
    m.insert("rebuild.done", "{n} fichier(s) reconstruit(s). Sortie : {path}");
    m.insert("rebuild.no_files", "Aucun fichier modifié trouvé dans l'espace de travail.");
    m.insert("rebuild.dup_found", "Les fichiers .yaml et .sbyml existent tous deux pour '{stem}'. Lequel utiliser ?");
    m.insert("rebuild.dup_sbyml", "Utiliser .sbyml (BYML natif)");
    m.insert("rebuild.dup_yaml", "Utiliser .yaml (YAML éditable)");

    m.insert("restore.title", "Restaurer un mod");
    m.insert("restore.select_prompt", "Choisissez un espace de travail à restaurer :");
    m.insert("restore.confirm", "Restaurer '{mod_name}' depuis la sauvegarde ?");
    m.insert("restore.confirm_details", "Cela écrasera le mod dans UKMM avec la sauvegarde originale.");
    m.insert("restore.done", "{mod_name} restauré à l'original.");
    m.insert("restore.no_backup", "Aucune sauvegarde trouvée pour cet espace de travail.");

    m.insert("list.title", "Mods disponibles");
    m.insert("list.none", "Aucun mod trouvé.");
    m.insert("list.header", "{n} mod(s) trouvé(s) dans UKMM :\n");

    m.insert("info.title", "ukmm-extractool — Informations");
    m.insert("info.desc", "Extraire et reconstruire les fichiers de mods UKMM en YAML éditable et BYML natif.");
    m.insert("info.formats", "Formats supportés : .byml / .sbyml / .sarc / .ssarc / .bshop / .aamp / .sbshop / .bdemo / .bfarc / .pack");
    m.insert("info.pipeline", "Pipeline : fichier → décompresser → détecter le format → analyser → sérialiser YAML / BYML");
    m.insert("info.rebuild", "Reconstruction : YAML / .sbyml → format CBOR → compression zstd → injection dans le ZIP");

    m.insert("err.generic", "{msg}");
    m.insert("err.invalid_selection", "Sélection invalide.");
    m.insert("err.no_valid_files", "Aucun fichier valide n'a pu être traité.");
    m.insert("err.rebuild_failed", "La reconstruction a échoué : {msg}");
    m.insert("err.extract_failed", "L'extraction a échoué : {msg}");

    m
}
