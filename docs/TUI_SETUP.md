# NovaGPT TUI e setup guidato (Ratatui 0.29)

NovaGPT usa **tre schede**: **Overview / Panoramica**, **Activity / Attività**, **Settings / Impostazioni**. **La lingua predefinita della TUI è inglese**; l'italiano e la rilevazione automatica della lingua di sistema sono disponibili nelle impostazioni. La Panoramica presenta soltanto le azioni pertinenti allo stato corrente, con operazioni avanzate accessibili premendo `a`; Attività è dedicata ai log; le impostazioni sono suddivise in sezioni. Il workspace predefinito e' la directory di lavoro corrente; `--root` ne seleziona una diversa. Le impostazioni NovaGPT sono globali per account del sistema operativo e si trovano in `NovaGPT/config.json` nell'area dati privata dell'utente. Cambiare workspace non cambia le preferenze; `.novagpt/` puo' contenere un launcher locale, ma non viene letto per le impostazioni. CodexPro originale e i relativi dati non vengono modificati.

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

## Localizzazione / Language

Il modulo interno `novagpt-cli/src/i18n.rs` fornisce il catalogo bilingue dell'interfaccia; non serve un crate NovaGPT aggiuntivo. La dipendenza [sys-locale](https://crates.io/crates/sys-locale) rileva la lingua del sistema tramite le API della piattaforma (anche Windows). Le opzioni nel menu **Settings → General → Interface language** (o **Impostazioni → Generali → Lingua dell'interfaccia**) sono:

- **English**: valore predefinito, indipendente dalla lingua del sistema.
- **Italiano**: traduzione italiana esplicita.
- **Automatic / Automatico**: usa la lingua rilevata dal sistema; `it` e varianti (`it-IT`, `it_IT`) selezionano italiano, le altre lingue o un valore non disponibile ripiegano su inglese.

Seleziona il campo e premi **Enter** per cambiare lingua immediatamente; premi **s** per salvare la preferenza nel file globale `NovaGPT/config.json` (`"language": "english" | "italian" | "auto"`). I vecchi file senza il campo sono compatibili e aprono la TUI in inglese. Cambiare soltanto la lingua non richiede riavviare MCP. La localizzazione riguarda la console Ratatui: i nomi dei tool MCP, i payload JSON e gli argomenti CLI restano invariati, mentre messaggi grezzi provenienti da sottosistemi esterni possono usare la lingua originaria.

## Navigazione

| Tasto | Azione |
| --- | --- |
| `Tab` / `Shift+Tab` | Cambia scheda |
| `←` / `→` | Cambia scheda quando non e' attivo un campo o una modale |
| `↑` / `↓` | Seleziona le azioni o le impostazioni, oppure scorre i log in Attività |
| `Enter` | Esegue, modifica o conferma la selezione |
| `Esc` | Chiude modale, annulla edit o torna indietro |
| `q` | Esce; richiede conferma se ci sono modifiche non salvate |
| `?` | Aiuto |
| `Ctrl+C` | Esce e ripristina il terminale |
| `PgUp` / `PgDn`, `Home` / `End` | Scroll log |
| `1`-`4` | Scorciatoie rapide nella Panoramica: avvio MCP, arresto MCP, inventario VM, diagnostica |
| `s` | Salva da Impostazioni (richiede conferma) |
| `a` | Mostra/nascondi le operazioni avanzate nella Panoramica |
| `n` / `b` | Avanti / indietro nel setup guidato |

Durante l'editing, `Tab`, `q` e le frecce non spostano il focus. Invio applica dopo la validazione, Esc annulla. La voce selezionata e' evidenziata; una descrizione appare a fianco o sotto, se il terminale ha spazio sufficiente.

## Panoramica

La home mostra lo stato MCP **solo dopo handshake autenticato**, la porta locale, la configurazione di Chrome **senza fingere una verifica di login**, e lo stato ngrok confermato da un evento del tunnel. Le azioni principali cambiano fra server spento e acceso; premi **a** per gli strumenti avanzati (VM, sottoagenti, CLI). Lo schermo usa meno riquadri, e su terminali più piccoli si adatta a una sola colonna. Il prefisso tecnico Win32 dei percorsi canonici viene nascosto nella visualizzazione. Usa le frecce e Invio, non necessariamente i tasti numerici.

- **Avvia MCP HTTP:** avvia il server protetto su `127.0.0.1` usando la configurazione salvata; se **Ngrok automatico** e' ON avvia anche il tunnel, usando l'eventuale dominio salvato. Per un avvio soltanto locale tramite CLI: `novagpt http --no-ngrok`. Il token viene generato al primo avvio e non compare nei log.
- **Arresta MCP HTTP:** apre una conferma; interrompe solo il server creato dalla sessione TUI, non eventuali processi esterni.
- **Apri browser agenti (login manuale):** quando MCP è fermo, apri Chrome con il profilo dedicato NovaGPT dalla Panoramica. Prima abilita **ChatGPT Browser** in Impostazioni e salva con `s`; la voce resta visibile anche da browser disattivato, ma spiega come abilitarlo. Accedi o completa le verifiche manualmente nella finestra Chrome, quindi **chiudila** prima di avviare MCP/Ngrok dalla TUI. La TUI rimane reattiva e mostra quando la finestra è aperta; non consente un secondo avvio o il riavvio MCP durante il login. Se un altro processo usa il profilo, viene mostrato un avviso e il profilo non viene riutilizzato forzatamente. L'apertura non prova che il login abbia avuto successo: nessuna sessione o credenziale viene letta dalla TUI.
- **Diagnostica workspace:** controlli locali e registrazione delle directory autorizzate eseguiti fuori dal ciclo input.
- **Inventario VM:** legge lo store locale approvato se VM e' abilitata nella configurazione salvata.
- **Avvia MCP + ngrok / Mostra / copia URL ChatGPT:** avvia un ngrok posseduto dalla TUI e richiede un gesto esplicito per mostrare la credenziale nel link. Premi **C** nella finestra solo se desideri copiarlo; non c'e' copia automatica.
- **Operazioni VM / Sottoagenti / Client e CLI:** modali con gli strumenti **MCP realmente disponibili** e relativi comandi espliciti. Non simulano operazioni host, browser o subagenti. Con un client MCP: `vm` supporta `images/create/status/destroy`; gli strumenti `subagent_*` richiedono il browser dedicato configurato. L'import VM manuale rimane la CLI esplicita `novagpt vm-import --name NAME --source PATH`. Per stdio: `novagpt stdio`. Per HTTP da CLI: `novagpt http --port 8787`.

La TUI mostra il server come **pronto** solo dopo un handshake MCP autenticato che verifica l'identita' di NovaGPT, non per la sola presenza di una porta aperta. Ngrok si avvia solo dopo che NovaGPT ha prenotato la porta TCP. La TUI può avviare un tunnel ngrok su richiesta: scegli **Avvia MCP + ngrok**, poi **Mostra / copia URL ChatGPT**. L'URL completo compare in una finestra riservata solo dopo un handshake MCP autenticato e la verifica del tunnel; premi **C** per copiarlo esplicitamente, oppure chiudi la finestra senza toccare gli appunti. La connessione al ChatGPT remoto non è ancora verificata. Non e' presente alcun pannello web.

La shell MCP di NovaGPT è **sempre disponibile senza modalità**: sono stati rimossi `BashMode`, `--bash-mode` e `--allow-full-shell`. La TUI e i server stdio/HTTP utilizzano la shell host (PowerShell, CMD, Bash o WSL, dove disponibili). Il browser resta indipendente e si abilita tramite **ChatGPT Browser** nella configurazione. Per login manuale: `novagpt --root C:\Projects\novagpt browser-login`. **ATTENZIONE:** la shell esegue comandi con i permessi dell'utente NovaGPT, anche fuori dal workspace; i limiti di tempo e output non costituiscono una sandbox. Qualsiasi client MCP HTTP autenticato, anche via ngrok, può eseguire comandi sul computer. Non condividere il token e non esporre il servizio a client non fidati. Gli strumenti Git, patch e import rimangono MCP separati; Git eredita la configurazione globale/di sistema senza modificarla.

## Token HTTP automatico o personalizzato

Per salvare un token **scelto da te**, senza generarlo casualmente, usa il comando seguente (richiede 64 caratteri esadecimali):

```powershell
novagpt token set --replace
```

La CLI chiede il valore in modalita' **nascosta**, senza inserirlo nella riga di comando o mostrarlo nei log. Il token resta associato all'utente del sistema operativo ed e' condiviso da tutti i suoi workspace NovaGPT, anche dopo il riavvio. Se ancora non esiste un token puoi omettere `--replace`; se ne esiste gia' uno, `--replace` rende esplicita la sostituzione. Per gli script: `novagpt token set --stdin --replace`, fornendo una sola riga alla pipe di input standard (mai come argomento CLI). Il programma non riproduce il valore ricevuto nell'output.

Il token continua a essere conservato nello store privato dell'utente, non nel file globale `NovaGPT/config.json`. La variabile `NOVAGPT_HTTP_TOKEN`, se impostata, ha sempre precedenza: rimuovila dall'ambiente prima di usare il token appena salvato. Tutti i server HTTP NovaGPT gia' in esecuzione vanno riavviati per leggere il nuovo valore. Anche gli URL dei connettori ChatGPT devono essere aggiornati se il token cambia.

Se la variabile `NOVAGPT_HTTP_TOKEN` non e' definita, la TUI, il setup confermato e il comando `novagpt http` creano (solo al primo utilizzo) e riutilizzano un unico token casuale per tutti i workspace dell'utente. La variabile d'ambiente, se presente, **ha precedenza**: se non valida, HTTP rifiuta l'avvio invece di ripiegare sul token salvato. La TUI non inserisce il token nei log: la voce **Genera / verifica token HTTP condiviso** mostra solo stato e percorso. La credenziale appare solo se scegli volontariamente **Mostra / copia URL ChatGPT** e puoi nascondere il link chiudendo la finestra.

La chiave e' salvata **fuori dai repository**, nell'area dati privata dell'utente: `%LOCALAPPDATA%\\NovaGPT\\tokens\\http.token` su Windows, `~/.local/state/NovaGPT/tokens/http.token` su Unix (oppure `XDG_STATE_HOME`). Il token non dipende dal percorso del workspace. Al primo utilizzo, se viene trovato un solo vecchio token lo conserva e rimuove il relativo file; se trova piu' vecchi token, ne genera uno nuovo e rimuove i vecchi file convalidati. File legacy non sicuri o malformati fermano la migrazione invece di essere cancellati. Un token gia' condiviso ha sempre la precedenza, anche se una precedente pulizia era stata interrotta. Le cartelle create da NovaGPT su Unix sono private (0700) e il file e' 0600; su Windows la protezione dipende anche dagli ACL della directory utente `%LOCALAPPDATA%`. File mancanti sono creati in modo non sovrascrivente; symlink e file malformati vengono respinti. Il token non e' importato da CodexPro e non viene mai incluso in `config.json`.

Per configurare un client MCP, esegui **localmente** nel terminale (mostra il segreto solo su richiesta):

```powershell
novagpt token show
novagpt token path
```

Il token e' richiesto dal server come credenziale di autenticazione MCP. Non condividere mai l'output di `token show`, non inserirlo in commit, messaggi o screenshot. Se cambi il token mentre HTTP e' in esecuzione, tutti i server NovaGPT vanno riavviati e i connettori che usano il vecchio token aggiornati. Il token condiviso concede l'accesso a tutti i workspace autorizzati dal server: trattalo come una credenziale per l'esecuzione di comandi host. La generazione non abilita di per se' l'accesso dalla rete: il server resta in ascolto solo su loopback.

Le operazioni CLI di ispezione e diagnostica sono disponibili senza TUI: `novagpt doctor --json`, `novagpt inspect --json`, `novagpt review --json`. `--log-format human|llm` seleziona rispettivamente testo standard o compatto, sempre in inglese e **mai JSON**, per la diagnostica **stderr** del server. L'audit MCP metadata-only ora viene salvato fuori dal repository (`%LOCALAPPDATA%\NovaGPT\logs\<hash-workspace>\` su Windows) anche per l'HTTP avviato dalla TUI. `runtime_status` espone gli ultimi eventi recuperati anche dopo un riavvio; con `novagpt logs path` e `novagpt logs tail --lines 50` puoi consultare localmente file di testo `audit.log` ruotati (4 x 64 KiB). Si registrano solo nome statico del tool, timestamp, sequenza ed esito; mai argomenti, token, path dei file o output. Non e' un registro completo delle chiamate/protocollo, e la TUI conserva separatamente il proprio buffer di eventi di lifecycle (massimo 250).

## Attività e log

La schermata Attività dedica la maggior parte dello spazio agli eventi: workspace, MCP/endpoint, tunnel, VM/browser (configurati, **non** dichiarati avviati), indicazione di riavvio e cronologia. I log INFO, WARN, ERROR e OK con timestamp UTC sono conservati in un buffer di massimo **250 eventi**. Si possono scorrere anche mentre arrivano nuovi eventi. Sono eventi di lifecycle/operazione della TUI, **non** la traccia completa delle invocazioni MCP; nessun token viene registrato.

## Impostazioni

I campi sono ora ordinati per **Connessione, Browser, Macchine virtuali, Directory autorizzate, Importazione e Applica modifiche**; la scheda presenta l'aiuto contestuale del campo selezionato nelle finestre ampie. `s` salva la bozza con conferma, indipendentemente dalla riga selezionata. Menu navigabile con porta HTTP, attivazione VM, attivazione ChatGPT Browser, percorso assoluto dell'eseguibile Chrome, aggiunta/rimozione di directory autorizzate, scelta profilo CodexPro, anteprima importazione, **Ngrok automatico (ON/OFF)**, **Dominio ngrok** modificabile, Salva, Annulla e **Genera / verifica token HTTP condiviso**. L'asterisco (`*`) indica i valori modificati. Errori di input, percorso Chrome non valido e salvataggio fallito sono visibili. Le directory devono esistere, sono canonicalizzate e limitate a 16 radici aggiuntive.

La voce **Eseguibile Chrome** puo' rimanere vuota: NovaGPT cerca automaticamente Chrome con lo stesso ordine di CodexPro (Windows: `ProgramFiles`, `ProgramFiles(x86)`, `LOCALAPPDATA`; macOS: cartelle Applications; Linux: `google-chrome`, `google-chrome-stable`, `chromium`, `chromium-browser` nel `PATH`). Un percorso esplicito resta prioritario e, se errato, non viene sostituito silenziosamente con quello automatico. La CLI accetta anche `NOVAGPT_CHROME_PATH` quando il browser e' abilitato. La scoperta riguarda soltanto l'eseguibile: NovaGPT usa sempre un **profilo Chrome dedicato**, distinto dai profili personali e da quelli CodexPro.

La configurazione viene mantenuta in **bozza** fino alla conferma del salvataggio e persiste nel file globale `NovaGPT/config.json`. Il segreto HTTP non viene scritto in `config.json`: e' conservato separatamente nell'area privata dell'utente. I nuovi valori salvati richiedono un riavvio HTTP se un task server usa ancora i precedenti.

## Setup `novagpt --setup`

Apre la **stessa TUI** inizialmente su Impostazioni. Segui il banner:

1. **Workspace:** verifica il percorso nell'header.
2. **Parametri:** configura i campi necessari.
3. **Import facoltativo:** scegli un profilo CodexPro e visualizza l'anteprima prima della conferma.
4. **Revisione:** salva solo dopo conferma esplicita; il risultato finale e' visualizzato. In questa fase NovaGPT genera automaticamente anche il token HTTP, salvo override tramite ambiente.

L'importazione da un altro workspace NovaGPT è disponibile direttamente nel menu **Config → Importa da cartella NovaGPT**, indicando un percorso assoluto, oppure all'avvio con `novagpt --novagpt-source C:\\path\\to\\other-workspace`. Legge in sola lettura il vecchio `<sorgente>/.novagpt/config.json` e propone **solo porta HTTP, stato ngrok e dominio ngrok**; anteprima, conferma e salvataggio sono separati. Non copia i file di progetto, i token, le autorizzazioni alle directory, VM, browser o altre impostazioni sensibili. La cartella sorgente non viene modificata. Si importa un workspace per volta e solo dopo conferma le preferenze selezionate confluiscono nelle impostazioni globali.

L'importazione CodexPro legge **porta HTTP**, preferenza `tunnel` e, solo quando `tunnel=ngrok` e l'hostname e' valido, il **dominio ngrok**. Importare un profilo con tunnel differente disabilita l'avvio automatico ngrok; un hostname ngrok mancante o non valido non abilita automaticamente il tunnel. L'anteprima e la conferma sono obbligatorie; la modifica resta **in bozza** fino a Salva. Token, file `ngrokConfig` e relativo contenuto, profili Chrome, credenziali, dati VM e permessi filesystem non vengono copiati. La directory sorgente (`~/.codexpro/profiles` o `--codexpro-home`) viene **solo letta**; CodexPro non viene modificato.

Il setup, dopo il salvataggio, installa in modo locale:

```text
<workspace>/.novagpt/
  bin/novagpt.exe      # Windows (novagpt su Unix)
  start.cmd            # Windows (start.sh su Unix)

<stato-utente>/NovaGPT/
  config.json          # impostazioni globali
  tokens/http.token    # token condiviso
  vms/                 # se abilitato
  browser/             # se abilitato
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


L'audit persistente registra esclusivamente testo inglese leggibile anche da LLM (`event=... timestamp_unix=... tool=... status=...`); i precedenti file `audit*.jsonl` restano archiviati e non vengono letti o modificati dalla nuova versione. La TUI è l'unica eccezione: gli stati e gli eventi presentati a schermo possono essere localizzati in inglese o italiano. Il JSON dei comandi diagnostici `--json` e del protocollo MCP non è un formato di logging.
