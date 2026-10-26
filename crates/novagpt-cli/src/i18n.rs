//! Localized TUI messages and system language detection.
//! English is the product default, Italian is supported explicitly,
//! and Auto resolves the platform UI language once per TUI session.
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LanguageChoice {
    #[default]
    English,
    Italian,
    Auto,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Language {
    English,
    Italian,
}

impl LanguageChoice {
    pub fn next(self) -> Self {
        match self {
            Self::English => Self::Italian,
            Self::Italian => Self::Auto,
            Self::Auto => Self::English,
        }
    }

    pub fn resolved(self, system: Language) -> Language {
        match self {
            Self::English => Language::English,
            Self::Italian => Language::Italian,
            Self::Auto => system,
        }
    }
}

pub fn detect_system() -> Language {
    language_from_tag(sys_locale::get_locale().as_deref())
}

pub fn language_from_tag(tag: Option<&str>) -> Language {
    match tag
        .and_then(|tag| tag.split(['-', '_', '.', '@']).next())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("it") => Language::Italian,
        _ => Language::English,
    }
}

/// Define typed localization methods at compile time. Each key requires both locales.
///
/// Static message: `Messages::main_page_header(lang)` -> `&'static str`.
/// Parameterized message: `Messages::vm_image_count(lang, count)` -> `String`.
/// Missing locale entries are compile errors; no runtime string replacement occurs.
macro_rules! messages {
    (
        static { $( $key:ident: { en: $en:literal, it: $it:literal }, )* }
        dynamic { $( $dynamic_key:ident ( $( $arg:ident : $ty:ty ),* $(,)? ): { en: $dynamic_en:literal, it: $dynamic_it:literal }, )* }
    ) => {
        pub struct Messages;
        #[allow(dead_code)]
        impl Messages {
            $(
                pub const fn $key(lang: Language) -> &'static str {
                    match lang { Language::English => $en, Language::Italian => $it }
                }
            )*
            $(
                pub fn $dynamic_key(lang: Language, $( $arg: $ty ),*) -> String {
                    match lang {
                        Language::English => format!($dynamic_en, $( $arg = $arg ),*),
                        Language::Italian => format!($dynamic_it, $( $arg = $arg ),*),
                    }
                }
            )*
        }
    };
}

messages! {
    static {
        nova_folder_verified: { en: "NovaGPT folder verified: review the preview.", it: "Cartella NovaGPT verificata: controlla l'anteprima." },
        invalid_chrome_binary: { en: "Chrome executable invalid or inaccessible.", it: "Eseguibile Chrome non valido o non accessibile." },
        save_before_http: { en: "Save changes before starting HTTP.", it: "Salva le modifiche prima di avviare HTTP." },
        stop_http_before_ngrok: { en: "Stop the HTTP server before starting ngrok.", it: "Arresta il server HTTP prima di avviare ngrok." },
        save_before_ngrok: { en: "Save settings before starting ngrok.", it: "Salva la configurazione prima di avviare ngrok." },
        no_import_profiles: { en: "No import profiles are available.", it: "Nessun profilo di importazione disponibile." },
        no_import_port: { en: "No importable port is available in this profile.", it: "Nessuna porta importabile nel profilo." },
        setup_review_step: { en: "Review step: confirm saving.", it: "Passo di revisione: conferma il salvataggio." },
        validation_saving: { en: "Validating and saving...", it: "Validazione e salvataggio in corso..." },
        notice_vm_body: { en: "MCP 'vm' tool: images, create (image), status (id), destroy (id, destructive). Approved disk import: novagpt vm-import --name N --source FILE [--cpus 2 --memory-mb 4096]. Host operations require an MCP client with explicit parameters.", it: "Tool MCP 'vm': images, create (image), status (id), destroy (id, distruttivo). Importazione di dischi approvati: novagpt vm-import --name N --source FILE [--cpus 2 --memory-mb 4096]. Le operazioni host richiedono un client MCP con parametri espliciti." },
        notice_agents_body: { en: "Available tools: subagent_spawn, subagent_status, subagent_message, subagent_result, subagent_cancel, subagent_resume, subagent_browser_diagnostics. An enabled ChatGPT browser, dedicated Chrome and manual login are required; this menu does not start automation.", it: "Tool disponibili: subagent_spawn, subagent_status, subagent_message, subagent_result, subagent_cancel, subagent_resume, subagent_browser_diagnostics. Servono browser ChatGPT abilitato, Chrome dedicato e autenticazione manuale; nessuna automazione e' avviata da questo menu." },
        notice_cli_body: { en: "MCP stdio: novagpt stdio\nHTTP via CLI: novagpt http --ngrok\nShow URL: novagpt connector-url\nVM import: novagpt vm-import --name N --source FILE", it: "MCP stdio: novagpt stdio\nHTTP da CLI: novagpt http --ngrok\nMostra URL: novagpt connector-url\nVM import: novagpt vm-import --name N --source FILE" },
        edit_novagpt_source_path: { en: "Source NovaGPT workspace path", it: "Percorso workspace NovaGPT sorgente" },
        confirm_import_port: { en: "Import supported HTTP port and ngrok preferences into the draft? Tokens will not be imported.", it: "Importare nella bozza porta HTTP e preferenze ngrok supportate? I token non saranno importati." },
        authorized_directories_help: { en: "Add or revoke other authorized working directories.", it: "Aggiungi o revoca altre directory di lavoro autorizzate." },
        import_connection_help: { en: "Import only connection settings after preview and confirmation.", it: "Importa solo impostazioni di connessione, dopo anteprima e consenso." },
        import_novagpt_help: { en: "Import public settings from another NovaGPT workspace.", it: "Importa la configurazione pubblica di un altro workspace NovaGPT." },
        next_setup_help: { en: "Continue to the next step of guided setup.", it: "Procedi al passo successivo del setup guidato." },
        setup_workspace: { en: "SETUP 1/4 · Workspace · n: next", it: "SETUP 1/4 · Workspace · n: avanti" },
        setup_optional_import: { en: "SETUP 3/4 · Optional import · n: next", it: "SETUP 3/4 · Importazione facoltativa · n: avanti" },
        settings_browser_heading: { en: "BROWSER", it: "BROWSER" },
        keys_overview: { en: "↑↓ actions   Enter run   a advanced   Tab views   ? help   q quit", it: "↑↓ azioni   Enter esegui   a avanzate   Tab schede   ? aiuto   q esci" },
        keys_activity: { en: "↑↓ / PgUp PgDn events   Tab views   ? help   q quit", it: "↑↓ / PgUp PgDn eventi   Tab schede   ? aiuto   q esci" },
        edit_input_prompt: { en: "Enter a value and press Enter:", it: "Inserisci il valore e premi Invio:" },
        none_server: { en: "Stopped", it: "Disattivato" },
        settings_save_pending: { en: "● Unsaved changes", it: "● Modifiche da salvare" },
        settings_saved_check: { en: "✓ Settings saved", it: "✓ Configurazione salvata" },
        edit_confirm_yes: { en: "[ YES ]", it: "[ SI ]" },
        edit_confirm_no: { en: "[ NO ]", it: "[ NO ]" },
        next_show_url: { en: "Select «Show / copy ChatGPT URL»", it: "Seleziona «Mostra / copia URL ChatGPT»" },
        next_check_tunnel: { en: "Local MCP ready · check the ChatGPT tunnel", it: "MCP locale pronto · verifica il tunnel per ChatGPT" },
        next_start_mcp: { en: "Select «Start MCP HTTP» to begin", it: "Seleziona «Avvia MCP HTTP» per iniziare" },
        actions_hide_advanced: { en: "Actions · a: hide advanced", it: "Azioni · a: nascondi avanzate" },
        warn_credential_url: { en: "Warning: this URL contains a credential that also grants host command execution via MCP. Do not share it.", it: "Attenzione: l'URL contiene una credenziale che permette anche di eseguire comandi host tramite MCP. Non condividerlo." },
        modal_confirm_short: { en: "←→ select · Enter confirm · Esc cancel", it: "←→ scegli · Enter conferma · Esc annulla" },
        terminal_too_small: { en: "NovaGPT · enlarge terminal\nCtrl+C to exit", it: "NovaGPT · amplia il terminale\nCtrl+C per uscire" },
        connector_http_port: { en: "HTTP port:", it: "Porta HTTP:" },
        endpoint_label: { en: "  Endpoint     ", it: "  Endpoint     " },
        browser_label: { en: "  Browser      ", it: "  Browser      " },
        tunnel_label: { en: "  Tunnel       ", it: "  Tunnel       " },
        vm_inventory: { en: "VM inventory", it: "Inventario VM" },
        vm_inventory_description: { en: "List approved images in the local NovaGPT store.", it: "Elenca le immagini approvate nello store NovaGPT locale." },
        subagents_description: { en: "Spawn/status/message/result/cancel/resume via MCP when browser is enabled.", it: "Spawn/status/message/result/cancel/resume via MCP, se browser attivo." },
        remove_authorized_directory: { en: "Remove authorized directory", it: "Rimuovi directory autorizzata" },
        help_body: { en: "Tab / Shift+Tab: Overview, Activity, Settings\n↑ ↓: select or scroll logs\nEnter: run, edit or confirm\na: show/hide advanced actions in Overview\ns: save in Settings\nEsc: close/back   q: quit\n?: help   Ctrl+C: safe exit\nActivity: PgUp/PgDn, Home/End scroll events\nSettings: * marks a changed value\nSetup: n next, b back\nKeys 1–4: quick actions.", it: "Tab / Shift+Tab: Panoramica, Attività, Impostazioni\n↑ ↓: seleziona o scorri i log\nEnter: esegui, modifica o conferma\na: mostra/nascondi azioni avanzate in Panoramica\ns: salva da Impostazioni\nEsc: chiudi/indietro   q: esci\n?: aiuto   Ctrl+C: uscita controllata\nAttività: PgUp/PgDn, Home/End scorrono gli eventi\nImpostazioni: * indica un valore modificato\nSetup: n avanti, b indietro\nScorciatoie 1–4: operazioni rapide." },
        choose_english_italian_or_automatic_system_language: { en: "Choose English, Italian or Automatic (system language).", it: "Scegli English, Italiano o Automatico (lingua di sistema)." },
        language_italian: { en: "Italian", it: "Italiano" },
        absent: { en: "(absent)", it: "(assente)" },
        only_port_ngrok_tunnel_and_valid_hostname: { en: "Only port, ngrok tunnel and valid hostname are imported. Tokens, ngrokConfig, roots, VM and browser data are not imported.", it: "Importa nella bozza solo porta, tunnel ngrok e hostname valido. Non importa token, file ngrokConfig, root, dati VM o browser." },
        no_changes_until_you_confirm_and_save: { en: "No changes until you confirm and save.", it: "Nessuna modifica finche' non confermi e salvi." },
        source: { en: "Source:", it: "Sorgente:" },
        profile: { en: "Profile", it: "Profilo" },
        profile_workspace: { en: "Profile workspace:", it: "Workspace profilo:" },
        source_tunnel: { en: "Source tunnel:", it: "Tunnel sorgente:" },
        importable_ngrok_hostname: { en: "Importable ngrok hostname:", it: "Dominio ngrok importabile:" },
        save_novagpt_settings_and_install_the_local: { en: "Save NovaGPT settings and install the local launcher? Existing files will not be overwritten.", it: "Salvare la configurazione NovaGPT e installare il launcher locale? Nessun file esistente sara' sovrascritto." },
        port_and_approved_ngrok_settings_imported_into: { en: "Port and approved ngrok settings imported into draft; save to apply.", it: "Porta e impostazioni ngrok consentite importate nella bozza; salva per applicarle." },
        c_copy_to_clipboard_enter_esc_close: { en: "C = copy to clipboard • Enter / Esc = close (never copied automatically)", it: "C = copia adesso negli appunti • Enter / Esc = chiudi (nessuna copia automatica)" },
        unable_to_import_folder_check_path_and: { en: "Unable to import folder: check path and .novagpt/config.json.", it: "Impossibile importare la cartella: verifica percorso e .novagpt/config.json." },
        prepare_novagpt_s_dedicated_browser_chatgpt_login: { en: "Prepare NovaGPT's dedicated browser. ChatGPT login is not verified.", it: "Prepara il browser dedicato NovaGPT. Non verifica l'accesso a ChatGPT." },
        save_local_changes_some_settings_require_restarting: { en: "Save local changes. Some settings require restarting MCP.", it: "Salva le modifiche locali. Alcune impostazioni richiedono riavvio MCP." },
        create_status_destroy_via_the_mcp_vm: { en: "Create/status/destroy via the MCP vm tool; explicit CLI import.", it: "Create/status/destroy via tool MCP vm; importazione via CLI esplicita." },
        start_the_server_and_public_tunnel_the: { en: "Start the server and public tunnel. The URL is not copied.", it: "Avvia il server e il tunnel pubblico. Non copia l'URL negli appunti." },
        http_token_using_novagpt_http_token_from: { en: "HTTP token: using NOVAGPT_HTTP_TOKEN from environment (hidden).", it: "Token HTTP: uso NOVAGPT_HTTP_TOKEN dall'ambiente (valore nascosto)." },
        fields_enter_edit_s_save_tab_views: { en: "↑↓ fields   Enter edit   s save   Tab views   ? help   q quit", it: "↑↓ campi   Enter modifica   s salva   Tab schede   ? aiuto   q esci" },
        optional_ngrok_hostname_do_not_enter_complete: { en: "Optional ngrok hostname. Do not enter complete URLs or credentials.", it: "Hostname ngrok opzionale. Non inserire URL completi o credenziali." },
        enable_optional_vm_features_requires_a_server: { en: "Enable optional VM features. Requires a server restart.", it: "Abilita le funzioni VM opzionali. Richiede un riavvio del server." },
        start_ngrok_with_mcp_when_this_setting: { en: "Start ngrok with MCP when this setting is enabled.", it: "Avvia ngrok assieme al server MCP quando la preferenza è attiva." },
        only_show_the_protected_url_on_request: { en: "Only show the protected URL on request; press C to copy.", it: "Mostra l'URL riservato solo su richiesta; premi C per copiarlo." },
        verify_or_generate_the_http_token_without: { en: "Verify or generate the HTTP token without displaying it.", it: "Verifica o genera il token HTTP senza mostrarlo sullo schermo." },
        startup_failed_settings_or_subsystems_unavailable: { en: "Startup failed: settings or subsystems unavailable.", it: "Avvio fallito: configurazione o sottosistemi non disponibili." },
        unable_to_list_vm_images_check_the: { en: "Unable to list VM images; check the local store.", it: "Impossibile leggere inventario VM; controlla lo store locale." },
        in_chatgpt_choose_no_authentication_and_paste: { en: "In ChatGPT choose No authentication and paste this URL:", it: "In ChatGPT scegli No authentication e incolla questo URL:" },
        start_the_local_mcp_server_with_the: { en: "Start the local MCP server with the configured HTTP token.", it: "Avvia il server MCP locale con il token HTTP configurato." },
        ngrok_unavailable_or_account_configuration_invalid: { en: "ngrok unavailable or account configuration invalid", it: "ngrok non disponibile o configurazione account non valida" },
        hidden_to_show_it_novagpt_token_show: { en: "(hidden). To show it: novagpt token show", it: "(valore nascosto). Per visualizzarlo: novagpt token show" },
        mcp_server_unavailable_or_authentication_failed: { en: "MCP server unavailable or authentication failed", it: "Server MCP non raggiungibile o autenticazione non valida" },
        mcp_restart_required_to_apply_settings: { en: "MCP restart required to apply settings.", it: "Riavvio MCP necessario per applicare la configurazione." },
        clipboard_unavailable_select_the_url_manually: { en: "Clipboard unavailable: select the URL manually", it: "Clipboard non disponibile: seleziona manualmente l'URL" },
        enter_a_valid_hostname_without_scheme_or: { en: "Enter a valid hostname without scheme or path.", it: "Inserisci un hostname valido, senza schema o percorso." },
        the_mcp_token_also_grants_access_to: { en: "  The MCP token also grants access to host commands.", it: "  Il token MCP concede accesso anche ai comandi host." },
        check_settings_directories_and_local_services: { en: "Check settings, directories and local services.", it: "Controlla configurazione, directory e servizi locali." },
        saved_changes_will_apply_at_the_next: { en: "Saved changes will apply at the next startup.", it: "Le modifiche salvate saranno usate al prossimo avvio." },
        stop_the_server_started_by_this_tui: { en: "Stop the server started by this TUI session.", it: "Interrompe il server avviato da questa sessione TUI." },
        chatgpt_url_shown_only_in_the_private: { en: "ChatGPT URL shown only in the private dialog.", it: "URL ChatGPT visualizzato solo in finestra riservata." },
        language_changed_save_to_keep_it_after: { en: "Language changed; save to keep it after restart.", it: "Lingua modificata; salva per mantenerla al riavvio." },
        ngrok_preference_changed_save_to_confirm: { en: "ngrok preference changed; save to confirm.", it: "Preferenza ngrok modificata; salva per confermare." },
        save_settings_in_novagpt_config_json: { en: "Save settings in .novagpt/config.json?", it: "Salvare la configurazione in .novagpt/config.json?" },
        stop_the_http_server_started_by_this: { en: "Stop the HTTP server started by this TUI?", it: "Interrompere il server HTTP avviato da questa TUI?" },
        value_changed_save_to_persist_it: { en: "Value changed; save to persist it.", it: "Valore modificato; salva per renderlo persistente." },
        revoke_this_authorized_directory_from_the_draft: { en: "Revoke this authorized directory from the draft?", it: "Revocare questa directory autorizzata dalla bozza?" },
        leave_blank_to_detect_chrome_automatically: { en: "Leave blank to detect Chrome automatically.", it: "Lascia vuoto per rilevare Chrome automaticamente." },
        port_on_which_novagpt_exposes_mcp_locally: { en: "Port on which NovaGPT exposes MCP locally.", it: "Porta sulla quale NovaGPT espone MCP localmente." },
        stdio_access_external_http_and_vm_import: { en: "stdio access, external HTTP and vm-import command.", it: "Accesso stdio, HTTP esterno e comando vm-import." },
        settings_saved_mcp_restart_required: { en: " Settings saved: MCP restart required", it: " Configurazione salvata: riavvio MCP necessario" },
        vm_is_disabled_in_the_saved_configuration: { en: "VM is disabled in the saved configuration.", it: "VM non abilitata nella configurazione salvata." },
        discard_all_unsaved_changes: { en: "Discard all unsaved changes.", it: "Scarta tutte le modifiche non ancora salvate." },
        authorized_directory_added_not_saved: { en: "Authorized directory added (not saved).", it: "Directory autorizzata aggiunta (non salvata)." },
        restart_http_to_apply_the_settings: { en: "Restart HTTP to apply the settings.", it: "Riavvia HTTP per applicare la configurazione." },
        audit_storage_is_unavailable_or_unsafe: { en: "Audit storage is unavailable or unsafe.", it: "Archivio audit non disponibile o non sicuro." },
        enter_a_value_and_press_enter: { en: "Enter a value and press Enter:\n\n{}▏\n\n{}", it: "Inserisci il valore e premi Invio:\n\n{}▏\n\n{}" },
        settings_select_enter_edit: { en: "Settings · ↑↓ select  Enter edit", it: "Impostazioni · ↑↓ seleziona  Enter modifica" },
        select_enter_confirm_esc_cancel: { en: "↑↓ select • Enter confirm • Esc cancel", it: "↑↓ seleziona • Invio conferma • Esc annulla" },
        enter_to_confirm_esc_cancel: { en: "Enter to confirm • Esc cancel", it: "Invio prosegui alla conferma • Esc annulla" },
        http_stop_requested_task_cancelled: { en: "HTTP stop requested (task cancelled).", it: "Arresto HTTP richiesto (task cancellato)." },
        settings_no_restart_required: { en: " Settings: no restart required", it: " Configurazione: nessun riavvio richiesto" },
        discard_all_unsaved_changes_2: { en: "Discard all unsaved changes?", it: "Annullare tutte le modifiche non salvate?" },
        quit_and_lose_unsaved_changes: { en: "Quit and lose unsaved changes?", it: "Uscire perdendo le modifiche non salvate?" },
        invalid_or_inaccessible_directory: { en: "Invalid or inaccessible directory.", it: "Directory non valida o non accessibile." },
        url_copied_to_clipboard_on_request: { en: "URL copied to clipboard on request.", it: "URL copiato negli appunti su richiesta." },
        setup_2_4_settings_enter_edit: { en: "SETUP 2/4 · Settings · Enter: edit", it: "SETUP 2/4 · Parametri · Enter: modifica" },
        select_enter_confirm_esc_cancel_2: { en: "select  Enter confirm  Esc cancel", it: "seleziona  Enter conferma  Esc annulla" },
        unable_to_prepare_http_token: { en: "Unable to prepare HTTP token: ", it: "Impossibile preparare il token HTTP: " },
        warning_launcher_not_installed: { en: " WARNING: launcher not installed.", it: " ATTENZIONE: launcher non installato." },
        configured_session_unverified: { en: "Configured · session unverified", it: "Configurato · sessione non verificata" },
        warning_http_token_not_created: { en: " WARNING: HTTP token not created: ", it: " ATTENZIONE: token HTTP non creato: " },
        for_all_events_open_activity: { en: "For all events, open Activity.", it: "Per tutti gli eventi, apri Attività." },
        server_is_already_running_or_starting: { en: "Server is already running or starting.", it: "Server gia in esecuzione o in avvio." },
        waiting_for_mcp_server_verification: { en: "Waiting for MCP server verification…", it: "Attendi la verifica del server MCP…" },
        ngrok_tunnel_not_ready_yet: { en: "ngrok tunnel not ready yet", it: "Tunnel ngrok non ancora disponibile" },
        ngrok_tunnel_startup_requested: { en: "ngrok tunnel startup requested...", it: "Avvio del tunnel ngrok richiesto..." },
        enter_a_value_and_press_enter_2: { en: "Enter a value and press Enter:", it: "Inserisci il valore e premi Invio:" },
        choose_codexpro_novagpt_profile: { en: "Choose CodexPro / NovaGPT profile", it: "Scegli profilo CodexPro / NovaGPT" },
        maximum_16_authorized_directories: { en: "Maximum 16 authorized directories.", it: "Massimo 16 directory autorizzate." },
        maximum_16_additional_directories: { en: "Maximum 16 additional directories.", it: "Massimo 16 directory aggiuntive." },
        no_tui_server_to_stop: { en: "No TUI server to stop.", it: "Nessun server TUI da arrestare." },
        add_authorized_directory: { en: "Add authorized directory", it: "Aggiungi directory autorizzata" },
        import_profile_preview: { en: "Import profile (preview)", it: "Importa da profilo (anteprima)" },
        an_http_server_is_already_running: { en: "An HTTP server is already running.", it: "Un server HTTP e' gia avviato." },
        directory_removed_from_draft: { en: "Directory removed from draft.", it: "Directory rimossa dalla bozza." },
        unable_to_initialize_vm: { en: "Unable to initialize VM.", it: "Impossibile inizializzare VM." },
        restore_last_save: { en: "restore last save", it: "ripristina ultimo salvataggio" },
        no_additional_directories: { en: "No additional directories.", it: "Nessuna directory aggiuntiva." },
        configured_unverified: { en: "configured (unverified)", it: "configurato (non verificato)" },
        enter_confirm_esc_cancel: { en: "Enter confirm · Esc cancel", it: "Enter conferma · Esc annulla" },
        setup_4_4_confirm_and_save: { en: "SETUP 4/4 · Confirm and save", it: "SETUP 4/4 · Conferma e salva" },
        unable_to_initialize_browser: { en: "Unable to initialize browser.", it: "Browser non inizializzabile." },
        generate_verify_http_token: { en: "Generate / verify HTTP token", it: "Genera / verifica token HTTP" },
        http_token_unavailable: { en: "HTTP token unavailable:", it: "Token HTTP non disponibile:" },
        the_ngrok_tunnel_closed: { en: "The ngrok tunnel closed", it: "Il tunnel ngrok si è chiuso" },
        clipboard_write_failed: { en: "Clipboard write failed", it: "Scrittura clipboard fallita" },
        import_novagpt_folder: { en: "Import NovaGPT folder", it: "Importa da cartella NovaGPT" },
        actions_a_show_advanced: { en: "Actions · a: show advanced", it: "Azioni · a: mostra avanzate" },
        setup_n_next_b_back: { en: "Setup: n next, b back", it: "Setup: n avanti, b indietro" },
        enter_apply_esc_cancel: { en: "Enter apply • Esc cancel", it: "Invio applica • Esc annulla" },
        importable_ngrok_hostname_2: { en: "Importable ngrok hostname: ", it: "Dominio ngrok importabile: " },
        enter_apply_esc_cancel_2: { en: "Enter apply · Esc cancel", it: "Enter applica · Esc annulla" },
        browser_setting_changed: { en: "Browser setting changed.", it: "Opzione browser modificata." },
        invalid_settings: { en: "Invalid settings.", it: "Configurazione non valida." },
        show_chatgpt_url: { en: "Show / copy ChatGPT URL", it: "Mostra / copia URL ChatGPT" },
        configured_not_connected: { en: "Configured · not connected", it: "Configurato · non connesso" },
        invalid_audit_path: { en: "Invalid audit path.", it: "Percorso audit non valido." },
        directory_already_authorized: { en: "Directory already authorized.", it: "Directory gia autorizzata." },
        invalid_tunnel_url: { en: "Invalid tunnel URL", it: "URL del tunnel non valido" },
        automatic_ngrok_on_off: { en: "Automatic ngrok (ON/OFF)", it: "Ngrok automatico (ON/OFF)" },
        choose_source_folder: { en: "choose source folder", it: "scegli cartella sorgente" },
        enter_edit_or_run: { en: "Enter  Edit or run", it: "Enter  Modifica o esegui" },
        s_save_in_settings: { en: "s: save in Settings", it: "s: salva da Impostazioni" },
        esc_enter_to_close: { en: "Esc / Enter to close", it: "Esc / Enter per chiudere" },
        enter_esc_to_close: { en: "Enter / Esc to close", it: "Invio / Esc per chiudere" },
        ngrok_domain_hostname: { en: "ngrok domain (hostname)", it: "Dominio ngrok (hostname)" },
        reading_vm_inventory: { en: "Reading VM inventory...", it: "Lettura inventario VM..." },
        start_mcp_ngrok_first: { en: "Start MCP + ngrok first.", it: "Avvia prima MCP + ngrok." },
        enter_a_directory: { en: "Enter a directory.", it: "Inserisci una directory." },
        settings_saved: { en: "Settings saved: ", it: "Configurazione salvata: " },
        chrome_unavailable: { en: "Chrome unavailable.", it: "Chrome non disponibile." },
        enable_chatgpt_browser: { en: "Enable ChatGPT Browser", it: "Abilita ChatGPT Browser" },
        settings_language: { en: "Interface language", it: "Lingua dell'interfaccia" },
        mcp_and_component_status: { en: " MCP AND COMPONENT STATUS", it: " STATO MCP E COMPONENTI" },
        chatgpt_url_private: { en: "ChatGPT URL (private)", it: "URL ChatGPT (riservato)" },
        diagnostics_workspace: { en: "Diagnostics: workspace ", it: "Diagnostica: workspace " },
        save_rejected: { en: "Save rejected: ", it: "Salvataggio rifiutato: " },
        settings_saved_2: { en: "Settings saved", it: "Configurazione salvata" },
        clipboard_operation_failed: { en: "Clipboard operation failed", it: "Clipboard non riuscito" },
        main_page_header: { en: "Service overview", it: "Panoramica dei servizi" },
        s_save_changes: { en: "s      Save changes", it: "s      Salva modifiche" },
        import_preview: { en: "Import preview", it: "Anteprima importazione" },
        vm_setting_changed: { en: "VM setting changed.", it: "Opzione VM modificata." },
        http_server_stopped: { en: "HTTP server stopped.", it: "Server HTTP terminato." },
        unsaved_changes: { en: "Unsaved changes", it: "Modifiche non salvate" },
        invalid_workspace: { en: "Invalid workspace.", it: "Workspace non valido." },
        clipboard_input_unavailable: { en: "Clipboard input unavailable", it: "Clipboard senza stdin" },
        token_unavailable: { en: "Token unavailable", it: "Token non disponibile" },
        workspace_diagnostics: { en: "Workspace diagnostics", it: "Diagnostica workspace" },
        invalid_executable: { en: "Invalid executable", it: "Eseguibile non valido" },
        authorized_directories: { en: "AUTHORIZED DIRECTORIES", it: "DIRECTORY AUTORIZZATE" },
        unsaved_changes_2: { en: "Unsaved changes", it: "Modifiche da salvare" },
        save_settings: { en: "Save settings", it: "Salva configurazione" },
        next_setup_step: { en: "Next setup step", it: "Prossimo passo setup" },
        automatic_system: { en: "Automatic (system:", it: "Automatico (sistema:" },
        automatic_detection: { en: "(automatic detection)", it: "(ricerca automatica)" },
        pending_changes: { en: "pending changes", it: "modifiche in sospeso" },
        c_copy_esc_close: { en: "C copy · Esc close", it: "C copia · Esc chiudi" },
        operation_in_progress: { en: "Operation in progress.", it: "Operazione in corso." },
        changes_discarded: { en: "Changes discarded.", it: "Modifiche annullate." },
        approved_images: { en: " approved images.", it: " immagini approvate." },
        mcp_server_stopped: { en: "MCP server stopped", it: "Server MCP terminato" },
        browser_disabled: { en: "Browser disabled", it: "Browser disattivato" },
        edit_http_port: { en: "Edit HTTP port", it: "Modifica porta HTTP" },
        profile_workspace_2: { en: "Profile workspace: ", it: "Workspace profilo: " },
        tunnel_available: { en: "Tunnel available", it: "Tunnel disponibile" },
        confirmation_required: { en: "Confirmation required", it: "Conferma richiesta" },
        available_tools: { en: "Available tools: ", it: "Tool disponibili: " },
        to_display_it: { en: "To display it:", it: "Per visualizzarlo:" },
        start_mcp_ngrok: { en: "Start MCP + ngrok", it: "Avvia MCP + ngrok" },
        chrome_executable: { en: "Chrome executable", it: "Eseguibile Chrome" },
        discard_changes: { en: "Discard changes", it: "Annulla modifiche" },
        unavailable: { en: "(unavailable)", it: "(non disponibile)" },
        virtual_machines: { en: "VIRTUAL MACHINES", it: "MACCHINE VIRTUALI" },
        apply_changes: { en: "APPLY CHANGES", it: "APPLICA MODIFICHE" },
        selected_field: { en: "SELECTED FIELD", it: "CAMPO SELEZIONATO" },
        remove_directory: { en: "Remove directory", it: "Rimuovi directory" },
        source_tunnel_2: { en: "Source tunnel: ", it: "Tunnel sorgente: " },
        ctrl_c_to_exit: { en: "Ctrl+C to exit", it: "Ctrl+C per uscire" },
        approval_required: { en: "approval required", it: "solo con consenso" },
        stop_mcp_http: { en: "Stop MCP HTTP", it: "Arresta MCP HTTP" },
        recent_activity: { en: " RECENT ACTIVITY", it: " ULTIMA ATTIVITÀ" },
        setup_complete: { en: "SETUP COMPLETE", it: "SETUP COMPLETATO" },
        mcp_subagents: { en: "MCP subagents", it: "Sottoagenti MCP" },
        server_failed: { en: "Startup error", it: "Errore di avvio" },
        not_set: { en: "(not set)", it: "(non impostato)" },
        not_configured: { en: "Not configured", it: "Non configurato" },
        tunnel_required: { en: "Tunnel required", it: "Richiede tunnel" },
        setup_complete_2: { en: "Setup complete", it: "Setup terminato" },
        chrome_path: { en: "Chrome path", it: "Percorso Chrome" },
        no_profiles_available: { en: "No profiles available.", it: "Nessun profilo." },
        vm_inventory_prefix: { en: "VM inventory: ", it: "Inventario VM: " },
        unavailable_2: { en: "unavailable", it: "non disponibile" },
        start_mcp_http: { en: "Start MCP HTTP", it: "Avvia MCP HTTP" },
        server_starting: { en: "Starting", it: "Avvio in corso" },
        current_values: { en: "current values", it: "valori attuali" },
        not_verified: { en: "Not verified", it: "Non verificato" },
        vm_disabled: { en: "VM disabled", it: "VM disattivata" },
        unverified: { en: "unverified", it: "non verificato" },
        tab_settings: { en: " Settings ", it: " Impostazioni " },
        vm_inventory_2: { en: "VM inventory", it: "Inventario VM" },
        vm_operations: { en: "VM operations", it: "Operazioni VM" },
        ngrok_hostname: { en: "ngrok hostname", it: "Dominio ngrok" },
        to_scroll: { en: " to scroll", it: " per scorrere" },
        guided_setup: { en: "GUIDED SETUP", it: "SETUP GUIDATO" },
        clients_and_cli: { en: "Clients and CLI", it: "Client e CLI" },
        import: { en: "IMPORT", it: "IMPORTAZIONE" },
        quick_help: { en: "Quick help", it: "Aiuto rapido" },
        tab_overview: { en: " Overview ", it: " Panoramica " },
        http_via_cli: { en: "HTTP via CLI:", it: "HTTP da CLI:" },
        disabled: { en: "disabled", it: "disabilitata" },
        disabled_2: { en: "disabled", it: "disabilitato" },
        subagents: { en: "Subagents", it: "Sottoagenti" },
        server_stopped: { en: "Stopped", it: "Non avviato" },
        port: { en: "    Port: ", it: "    Porta: " },
        settings_connection: { en: "CONNECTION", it: "CONNESSIONE" },
        shortcuts: { en: "SHORTCUTS", it: "SCORCIATOIE" },
        cli_access: { en: "CLI access", it: "Accesso CLI" },
        show_url: { en: "Show URL:", it: "Mostra URL:" },
        not_saved: { en: "not saved", it: "non salvata" },
        http_port: { en: "HTTP port", it: "Porta HTTP" },
        enable_vm: { en: "Enable VM", it: "Abilita VM" },
        server_ready: { en: "MCP ready", it: "MCP pronto" },
        source_2: { en: "Source: ", it: "Sorgente: " },
        tab_activity: { en: " Activity ", it: " Attività " },
        completed: { en: "Completed", it: "Completato" },
        automatic: { en: "Automatic", it: "Automatico" },
        invalid: { en: "INVALID", it: "NON VALIDO" },
        disabled_3: { en: "Disabled", it: "Disattivo" },
        none: { en: "(none)", it: "(nessuno)" },
        events: { en: "Events · ", it: "Eventi · " },
        in_progress: { en: "IN PROGRESS", it: "IN CORSO" },
        profile_2: { en: "Profile ", it: "Profilo " },
        settings_general: { en: "GENERAL", it: "GENERALI" },
        port_2: { en: "Port: ", it: "Porta: " },
        enabled: { en: "Enabled", it: "Attivo" },
        ready: { en: "Ready", it: "Pronto" },
        ready_2: { en: "ready", it: "pronto" },
        warning: { en: "WARNING", it: "AVVISO" },
        error: { en: "ERROR", it: "ERRORE" },
        port_3: { en: "port ", it: "porta " },
        step: { en: "step ", it: "passo " },
        error_2: { en: "Error", it: "Errore" },
        of: { en: " of ", it: " di " },
    }
    dynamic {
        vm_image_count(count: usize): { en: "VM inventory: {count} approved images.", it: "Inventario VM: {count} immagini approvate." },
        http_starting(port: u16): { en: "HTTP startup requested on port {port}...", it: "Avvio HTTP richiesto sulla porta {port}..." },
        event_count(current: usize, total: usize): { en: "Events · {current} of {total} · ↑↓ PgUp/PgDn to scroll", it: "Eventi · {current} di {total} · ↑↓ PgUp/PgDn per scorrere" },
        status_summary(server: &str, port: u16, tunnel: &str): { en: " MCP: {server}    Port: {port}    ngrok: {tunnel}", it: " MCP: {server}    Porta: {port}    ngrok: {tunnel}" },
        component_summary(chrome: &str, vm: &str): { en: " Chrome: {chrome}    VM: {vm}", it: " Chrome: {chrome}    VM: {vm}" },
        directory_count(count: usize): { en: "{count} directories", it: "{count} directory" },
        http_port_approval(port: u16): { en: "port {port}, approval required", it: "porta {port}, solo con consenso" },
        setup_step(step: usize): { en: "step {step}/4", it: "passo {step}/4" },
        auto_language(system: &str): { en: "Automatic (system: {system})", it: "Automatico (sistema: {system})" },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_configuration_defaults_to_english_and_rejects_invalid_languages() {
        let legacy: crate::settings::LocalSettings =
            serde_json::from_str(r#"{"version":1,"port":8787}"#).expect("legacy settings");
        assert_eq!(legacy.language, LanguageChoice::English);
        let mut next = legacy;
        next.language = LanguageChoice::Auto;
        let json = serde_json::to_string(&next).expect("serialized settings");
        let read: crate::settings::LocalSettings = serde_json::from_str(&json).expect("round-trip");
        assert_eq!(read.language, LanguageChoice::Auto);
        assert!(
            serde_json::from_str::<crate::settings::LocalSettings>(
                r#"{"version":1,"port":8787,"language":"german"}"#
            )
            .is_err()
        );
    }

    #[test]
    fn english_catalog_localizes_actions_and_italian_remains_unchanged() {
        assert_eq!(
            Messages::start_mcp_http(Language::English),
            "Start MCP HTTP"
        );
        assert_eq!(
            Messages::start_mcp_http(Language::Italian),
            "Avvia MCP HTTP"
        );
        assert_eq!(
            Messages::main_page_header(Language::English),
            "Service overview"
        );
        assert_eq!(Messages::settings_general(Language::English), "GENERAL");
    }
    #[test]
    fn locales_are_normalized_and_unsupported_tags_fall_back_to_english() {
        assert_eq!(language_from_tag(Some("it-IT")), Language::Italian);
        assert_eq!(language_from_tag(Some("IT_it.UTF-8")), Language::Italian);
        assert_eq!(language_from_tag(Some("en-US")), Language::English);
        assert_eq!(language_from_tag(Some("fr-FR")), Language::English);
        assert_eq!(language_from_tag(None), Language::English);
    }
    #[test]
    fn language_selection_default_and_cycle() {
        assert_eq!(LanguageChoice::default(), LanguageChoice::English);
        assert_eq!(LanguageChoice::English.next(), LanguageChoice::Italian);
        assert_eq!(LanguageChoice::Italian.next(), LanguageChoice::Auto);
        assert_eq!(LanguageChoice::Auto.next(), LanguageChoice::English);
        assert_eq!(
            LanguageChoice::Auto.resolved(Language::Italian),
            Language::Italian
        );
        assert_eq!(
            LanguageChoice::English.resolved(Language::Italian),
            Language::English
        );
    }
}
