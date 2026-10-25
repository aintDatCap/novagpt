# NovaGPT TUI e setup guidato (Ratatui 0.29)

NovaGPT usa **tre schede**: **Comandi**, **Stato**, **Config**. Il workspace predefinito e' la directory di lavoro corrente; `--root` ne seleziona una diversa. Le configurazioni NovaGPT sono locali in `<workspace>/.novagpt/`. CodexPro originale e i relativi dati non vengono modificati.

## PowerShell / Windows Terminal

```powershell
cd C:\Projects\novagpt
cargo build --release -p novagpt-cli
# Dal workspace di interesse
C:\Projects\novagpt\target\release\novagpt.exe
C:\Projects\novagpt\target\release\novagpt.exe --setup
C:\Projects\novagpt\target\release\novagpt.exe --root C:\Projects\my-workspace --setup
```

Richiede un terminale interattivo con raw mode e alternate screen; il setup non interattivo non crea file. Gli eventi Crossterm di tipo `Press` e `Repeat` sono gestiti, `Release` ignorati; su resize avviene un nuovo layout.

## Navigazione

| Tasto | Azione |
| --- | --- |
| `Tab` / `Shift+Tab` | Cambia scheda |
| `←` / `→` | Cambia scheda quando non e' attivo un campo o una modale |
| `↑` / `↓` | Seleziona menu/config oppure scorre i log in Stato |
| `Enter` | Esegue, modifica o conferma la selezione |
| `Esc` | Chiude modale, annulla edit o torna indietro |
| `q` | Esce; richiede conferma se ci sono modifiche non salvate |
| `?` | Aiuto |
| `Ctrl+C` | Esce e ripristina il terminale |
| `PgUp` / `PgDn`, `Home` / `End` | Scroll log |
| `1`-`4` | Scorciatoie opzionali nei Comandi: avvio HTTP, stop HTTP, inventario VM, diagnostica |
| `s` | Seleziona il salvataggio in Config (richiede conferma) |
| `n` / `b` | Avanti / indietro nel setup guidato |

Durante l'editing, `Tab`, `q` e le frecce non spostano il focus. Invio applica dopo la validazione, Esc annulla. La voce selezionata e' evidenziata; una descrizione appare a fianco o sotto, se il terminale ha spazio sufficiente.

## Comandi

Usa le frecce e Invio, non necessariamente i tasti numerici.

- **Avvia MCP HTTP:** server protetto sulla sola interfaccia di loopback `127.0.0.1`, con impostazioni precedentemente salvate. Se non esiste una variabile `NOVAGPT_HTTP_TOKEN`, genera automaticamente un token crittograficamente sicuro al primo avvio e lo riutilizza nelle sessioni successive. Non lo mostra nei log.
- **Arresta MCP HTTP:** apre una conferma; interrompe solo il server creato dalla sessione TUI, non eventuali processi esterni.
- **Diagnostica workspace:** controlli locali e registrazione delle directory autorizzate eseguiti fuori dal ciclo input.
- **Inventario VM:** legge lo store locale approvato se VM e' abilitata nella configurazione salvata.
- **Operazioni VM / Sottoagenti / Client e CLI:** modali con gli strumenti **MCP realmente disponibili** e relativi comandi espliciti. Non simulano operazioni host, browser o subagenti. Con un client MCP: `vm` supporta `images/create/status/destroy`; gli strumenti `subagent_*` richiedono il browser dedicato configurato. L'import VM manuale rimane la CLI esplicita `novagpt vm-import --name NAME --source PATH`. Per stdio: `novagpt stdio`. Per HTTP da CLI: `novagpt http --port 8787`.

L'indicazione **TCP raggiungibile** in Stato e' un probe locale; non e' un handshake MCP autenticato. La TUI non avvia o visualizza tunnel: ngrok e' disponibile soltanto con l'avvio CLI esplicito `novagpt http --ngrok` e non ha ancora una verifica ChatGPT remota. Non e' presente alcun pannello web.

La TUI abilita il set iniziale di comandi Rust Bash in modalita' `safe`: solo verifiche Rust predefinite, senza interpretazione shell. Le opzioni CLI `--bash-mode off` e `--bash-mode full --allow-full-shell` si applicano all'avvio esplicito `stdio`/`http` e non sono ancora un controllo della TUI. Gli strumenti Git, patch e import sono invocabili tramite MCP e non costituiscono menu TUI dedicati.

## Token HTTP automatico

Se la variabile `NOVAGPT_HTTP_TOKEN` non e' definita, la TUI, il setup confermato e il comando `novagpt http` creano (solo al primo utilizzo) e riutilizzano un token casuale per quel workspace. La variabile d'ambiente, se presente, **ha precedenza**: se non valida, HTTP rifiuta l'avvio invece di ripiegare sul token salvato. La TUI non mostra mai il token; la voce **Genera / verifica token HTTP** in Config visualizza solo stato e percorso.

La chiave e' salvata **fuori dal repository**, nell'area dati dell'utente: `%LOCALAPPDATA%\\NovaGPT\\tokens\\<hash-workspace>.token` su Windows, `~/.local/state/NovaGPT/tokens/<hash-workspace>.token` su Unix (oppure `XDG_STATE_HOME`). Il nome del file e' l'hash SHA-256 del percorso canonico del workspace. Le cartelle create da NovaGPT su Unix sono private (0700) e il file e' 0600; su Windows la protezione dipende anche dagli ACL della directory utente `%LOCALAPPDATA%`. File mancanti sono creati in modo non sovrascrivente; symlink e file malformati vengono respinti. Il token non e' importato da CodexPro e non viene mai incluso in `config.json`.

Per configurare un client MCP, esegui **localmente** nel terminale (mostra il segreto solo su richiesta):

```powershell
novagpt --root C:\Projects\my-workspace token show
novagpt --root C:\Projects\my-workspace token path
```

Il token e' richiesto dal server come credenziale di autenticazione MCP. Non condividere mai l'output di `token show`, non inserirlo in commit, messaggi o screenshot. Se cambi il token mentre HTTP e' in esecuzione, il server deve essere riavviato per applicare la modifica. La generazione non abilita di per se' l'accesso dalla rete: il server resta in ascolto solo su loopback.

Le operazioni CLI di ispezione e diagnostica sono disponibili senza TUI: `novagpt doctor --json`, `novagpt inspect --json`, `novagpt review --json`. `--log-format human|llm` seleziona il formato diagnostico **stderr** del server. L'audit MCP metadata-only ora viene salvato fuori dal repository (`%LOCALAPPDATA%\NovaGPT\logs\<hash-workspace>\` su Windows) anche per l'HTTP avviato dalla TUI. `runtime_status` espone gli ultimi eventi recuperati anche dopo un riavvio; con `novagpt logs path` e `novagpt logs tail --lines 50 [--json]` puoi consultare localmente file JSONL ruotati (4 x 64 KiB). Si registrano solo nome statico del tool, timestamp, sequenza ed esito; mai argomenti, token, path dei file o output. Non e' un registro completo delle chiamate/protocollo, e la TUI conserva separatamente il proprio buffer di eventi di lifecycle (massimo 250).

## Stato e log

La dashboard include workspace, stato del server e endpoint, VM/browser (configurati, **non** dichiarati avviati), ultimo errore noto e indicazione di riavvio. I log INFO, WARN, ERROR e OK con timestamp UTC sono conservati in un buffer di massimo **250 eventi**. Si possono scorrere anche mentre arrivano nuovi eventi. Sono eventi di lifecycle/operazione della TUI, **non** la traccia completa delle invocazioni MCP; nessun token viene registrato.

## Config

Menu navigabile con porta HTTP, attivazione VM, attivazione ChatGPT Browser, percorso assoluto dell'eseguibile Chrome, aggiunta/rimozione di directory autorizzate, scelta profilo CodexPro, anteprima importazione, Salva, Annulla e **Genera / verifica token HTTP**. L'asterisco (`*`) indica i valori modificati. Errori di input, percorso Chrome non valido e salvataggio fallito sono visibili. Le directory devono esistere, sono canonicalizzate e limitate a 16 radici aggiuntive.

La configurazione viene mantenuta in **bozza** fino alla conferma del salvataggio e persiste in `<workspace>/.novagpt/config.json`. Il segreto HTTP non viene scritto in `config.json`: e' conservato separatamente nell'area privata dell'utente. I nuovi valori salvati richiedono un riavvio HTTP se un task server usa ancora i precedenti.

## Setup `novagpt --setup`

Apre la **stessa TUI** inizialmente su Config. Segui il banner:

1. **Workspace:** verifica il percorso nell'header.
2. **Parametri:** configura i campi necessari.
3. **Import facoltativo:** scegli un profilo CodexPro e visualizza l'anteprima prima della conferma.
4. **Revisione:** salva solo dopo conferma esplicita; il risultato finale e' visualizzato. In questa fase NovaGPT genera automaticamente anche il token HTTP, salvo override tramite ambiente.

L'importazione CodexPro riguarda **esclusivamente la porta HTTP**. Profili Chrome, token, credenziali, dati VM, permessi filesystem e qualsiasi altro campo sono ignorati. La directory sorgente viene soltanto letta (di default `~/.codexpro/profiles`, personalizzabile con `CODEXPRO_HOME` o `--codexpro-home`). Non viene salvato niente automaticamente dopo l'importazione.

Il setup, dopo il salvataggio, installa in modo locale:

```text
<workspace>/.novagpt/
  config.json
  bin/novagpt.exe      # Windows (novagpt su Unix)
  start.cmd            # Windows (start.sh su Unix)
  vms/                 # se utilizzato
  browser/             # se utilizzato
```

I file launcher gia' esistenti non vengono **mai sovrascritti**; target non sicuri sono respinti. Se l'installazione del launcher fallisce dopo il salvataggio, la configurazione puo' essere comunque gia' stata scritta: il risultato lo segnala esplicitamente.

## Implementazione e limiti di verifica

I moduli includono `src/token.rs` (generazione, riuso, validazione e storage privato del token). Gli altri moduli sono `src/tui.rs` (RAII terminale, evento), `src/tui/model.rs` (stato e focus), `src/tui/render.rs` (ListState, TableState, Tabs, rendering), `src/tui/operations.rs` (Tokio e IO off-thread), `src/tui/launcher.rs` (installazione senza overwrite). Sono conservate Ratatui **0.29** e Crossterm **0.28** senza aggiornamenti di dipendenze. I test coprono input, conferme, salvataggio, importazione consenziente, rendering `TestBackend`, resize e cleanup simulato. **Non equivalgono a test interattivi reali PowerShell/Windows Terminal**, da effettuare separatamente, e non attestano la parita' con CodexPro.

Riferimenti ufficiali consultati:

- [Ratatui](https://ratatui.rs/) e [esempi](https://ratatui.rs/examples/)
- [Repository Ratatui](https://github.com/ratatui/ratatui) e [albero esempi](https://github.com/ratatui/ratatui/tree/main/examples)
- [Ratatui API 0.29](https://docs.rs/ratatui/0.29.0/ratatui/) e [TestBackend](https://docs.rs/ratatui/0.29.0/ratatui/backend/struct.TestBackend.html)
- [Crossterm keyboard/eventi 0.28](https://docs.rs/crossterm/0.28.1/crossterm/event/index.html) e [terminal lifecycle](https://docs.rs/crossterm/0.28.1/crossterm/terminal/index.html)

Consulta inoltre [VM e sottoagenti](VM_AND_SUBAGENTS.md), [parity](PARITY.md), [quality gates](QUALITY_GATES.md).
