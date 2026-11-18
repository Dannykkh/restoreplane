[CmdletBinding()]
param(
    [ValidateSet('all', 'workbench.baseline', 'workbench.bounds', 'workbench.slow_consumer', 'workbench.metrics')]
    [string]$Scenario = 'all',
    [string]$RunId = ('windows-' + (Get-Date -Format 'yyyyMMdd-HHmmss')),
    [switch]$ResumeBuild,
    [int]$DebugPort = 0
)
$ErrorActionPreference = 'Stop'
$taskRepo = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..'))
if ($env:OS -ne 'Windows_NT') { throw 'Actual Windows/WebView2 is required; browser mocks are not evidence.' }
if ($RunId -notmatch '^[A-Za-z0-9][A-Za-z0-9-]{0,95}$') { throw 'Invalid run ID.' }
$taskOutput = Join-Path $taskRepo ('artifacts\workbench-performance\' + $RunId)
if ((Test-Path -LiteralPath $taskOutput) -and -not $ResumeBuild) { throw 'Run exists; first results are immutable. Choose a new RunId.' }
if ($ResumeBuild -and (-not (Test-Path -LiteralPath $taskOutput) -or (Test-Path -LiteralPath (Join-Path $taskOutput 'phase.json')))) { throw 'ResumeBuild is only for an existing native-build attempt that has not started measurement.' }
$taskHarness = Join-Path $taskOutput 'native-harness'
$taskWeb = Join-Path $taskHarness 'web'
if (-not $ResumeBuild) { New-Item -ItemType Directory -Path $taskWeb,(Join-Path $taskHarness 'src'),(Join-Path $taskHarness 'icons') -Force | Out-Null }
$taskUtf8 = New-Object Text.UTF8Encoding($false)
function Write-TaskFile([string]$Path, [string]$Body) { [IO.File]::WriteAllText($Path, $Body, $taskUtf8) }
function Write-TaskJson([string]$Path, $Value) { Write-TaskFile $Path ($Value | ConvertTo-Json -Depth 32) }
if ($ResumeBuild) {
    $DebugPort = (Get-Content -Raw -LiteralPath (Join-Path $taskOutput 'environment.json') | ConvertFrom-Json).debugPort
    $taskRunner = Join-Path $taskOutput 'drive.cjs'
    $taskRequired = @('native-harness\src\main.rs','native-harness\Cargo.toml','native-harness\tauri.conf.json','native-harness\web\bench.js','drive.cjs')
    foreach ($taskRequiredFile in $taskRequired) { if (-not (Test-Path -LiteralPath (Join-Path $taskOutput $taskRequiredFile))) { throw ('Incomplete native staging: ' + $taskRequiredFile) } }
    Write-TaskJson (Join-Path $taskOutput 'resume-source-hashes.json') @($taskRequired | ForEach-Object { @{path=$_;sha256=(Get-FileHash -LiteralPath (Join-Path $taskOutput $_) -Algorithm SHA256).Hash} })
    $taskPriorSummary = Join-Path $taskOutput 'summary.json'
    if (Test-Path -LiteralPath $taskPriorSummary) { Copy-Item -LiteralPath $taskPriorSummary -Destination (Join-Path $taskOutput 'summary.before-build-resume.json') }
} else {
if ($DebugPort -eq 0) {
    $taskListener = New-Object Net.Sockets.TcpListener([Net.IPAddress]::Loopback, 0)
    $taskListener.Start(); $DebugPort = $taskListener.LocalEndpoint.Port; $taskListener.Stop()
}
$taskEnvironment = [ordered]@{
    runId=$RunId; scenario=$Scenario; createdAtUtc=[DateTime]::UtcNow.ToString('o');
    os=(Get-CimInstance Win32_OperatingSystem | Select-Object Caption,Version,BuildNumber);
    cpu=(Get-CimInstance Win32_Processor | Select-Object Name,NumberOfCores,NumberOfLogicalProcessors);
    physicalMemoryBytes=(Get-CimInstance Win32_ComputerSystem).TotalPhysicalMemory;
    gpu=(Get-CimInstance Win32_VideoController | Select-Object Name,DriverVersion,CurrentHorizontalResolution,CurrentVerticalResolution,CurrentRefreshRate);
    webviewInstalled=@(Get-ChildItem -LiteralPath 'C:\Program Files (x86)\Microsoft\EdgeWebView\Application' -Directory -ErrorAction SilentlyContinue | Where-Object Name -Match '^\d' | Select-Object -ExpandProperty Name);
    rust=(& rustc --version); node=(& node --version); gitRevision=(& git -C $taskRepo rev-parse HEAD);
    gitDirty=@(& git -C $taskRepo status --short); viewport=@{width=1240;height=840};
    debugPort=$DebugPort; transport='actual Tauri native invoke / WebView2 CDP';
    build='cargo build --manifest-path <run>/native-harness/Cargo.toml --target-dir <repo>/target';
    buildProfile='dev; Console dependency cache reused, standalone lockfile';
    timingMethod='M1: browser input event to React layout effect plus double requestAnimationFrame; automation arm-to-paint reported separately; frontend performance.now only; independent native monotonic timestamps are not subtracted; not physical display photometry';
    processMemoryMethod='Win32_Process descendant discovery and Get-Process WorkingSet64; summed shared working sets may double count shared pages';
    scope='Synthetic lazy native producer only; no real driver/DB/production permission or commit evidence'
}
Write-TaskJson (Join-Path $taskOutput 'environment.json') $taskEnvironment
Copy-Item -LiteralPath $PSCommandPath -Destination (Join-Path $taskOutput 'measure-workbench.source.ps1')
Copy-Item -LiteralPath (Join-Path $taskRepo 'apps\console\bench\workbench-performance.ts') -Destination (Join-Path $taskOutput 'workbench-performance.source.ts')
Copy-Item -LiteralPath (Join-Path $taskRepo 'Cargo.lock') -Destination (Join-Path $taskHarness 'Cargo.lock')
Copy-Item -LiteralPath (Join-Path $taskRepo 'apps\console\src-tauri\icons\icon.ico') -Destination (Join-Path $taskHarness 'icons\icon.ico')
Write-TaskFile (Join-Path $taskHarness 'Cargo.toml') @'
[workspace]
members = ["."]
[package]
name = "restoreplane-workbench-bench"
version = "0.0.0"
edition = "2024"
publish = false
[dependencies]
tauri = { version = "2", features = ["devtools"] }
serde = { version = "1", features = ["derive"] }
serde_json = "1"
[build-dependencies]
tauri-build = "2"
'@
Write-TaskFile (Join-Path $taskHarness 'build.rs') 'fn main() { tauri_build::build() }'
Write-TaskJson (Join-Path $taskHarness 'tauri.conf.json') @{
    productName='RestorePlane Workbench Benchmark'; version='0.0.0'; identifier='com.dannykkh.restoreplane.benchmark';
    build=@{frontendDist='web'};
    app=@{withGlobalTauri=$true;windows=@(@{label='main';title='데이터 관리 성능 검증';width=1240;height=840;resizable=$false;visible=$true;additionalBrowserArgs="--remote-debugging-port=$DebugPort --remote-allow-origins=* --disable-background-timer-throttling --disable-renderer-backgrounding"});security=@{csp="default-src 'self'; connect-src 'self' ipc: http://ipc.localhost; style-src 'self' 'unsafe-inline'; img-src 'self' data:; script-src 'self'"}};
    bundle=@{active=$false;icon=@('icons/icon.ico')}
}
Write-TaskFile (Join-Path $taskWeb 'index.html') '<!doctype html><html lang="ko"><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1"><title>데이터 관리 성능 검증</title><style>body{margin:0;background:#14181f;color:#edf1f6;font:13px Pretendard,Segoe UI,sans-serif}button{color:#edf1f6;background:#232b37;border:1px solid #3b4757;padding:8px}</style><div id="root"></div><script src="bench.js"></script></html>'
Write-TaskFile (Join-Path $taskHarness 'src\main.rs') @'
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
use serde::Serialize;
use serde_json::{Value, json};
use std::{collections::BTreeMap, sync::{Mutex, OnceLock}, time::Instant};
const FRAME: usize = 256 * 1024;
const CREDIT: usize = 8 * 1024 * 1024;
const ROWS: u32 = 100_000;
const COLUMNS: u32 = 100;
static START: OnceLock<Instant> = OnceLock::new();
struct Producer {
    generation: u64, seed: u64, sequence: u64, acknowledged: u64,
    ledger: BTreeMap<u64, usize>, outstanding: usize, peak: usize,
    terminal: String, generated_rows: u64, rejected_acks: u64, last_ack: Instant,
}
impl Default for Producer {
    fn default() -> Self { Self { generation:0,seed:0,sequence:0,acknowledged:0,ledger:BTreeMap::new(),outstanding:0,peak:0,terminal:"idle".into(),generated_rows:0,rejected_acks:0,last_ack:Instant::now() } }
}
#[derive(Serialize)]
#[serde(rename_all="camelCase")]
struct Status {
    generation:u64, sequence:u64, acknowledged:u64, outstanding_bytes:usize,
    peak_outstanding_bytes:usize, native_retained_bytes:usize, reserved_bytes:usize,
    terminal:String, generated_rows:u64, rejected_acks:u64, monotonic_micros:u64,
}
impl Producer {
    fn expire(&mut self) {
        if self.terminal == "streaming" && self.outstanding > 0 && self.last_ack.elapsed().as_millis() >= 5000 {
            self.terminal = "interrupted".into(); self.ledger.clear(); self.outstanding = 0;
        }
    }
    fn check(&self, generation:u64) -> Result<(),String> {
        if generation != self.generation { Err("STALE_GENERATION".into()) } else { Ok(()) }
    }
    fn status(&self) -> Status { Status {
        generation:self.generation,sequence:self.sequence,acknowledged:self.acknowledged,
        outstanding_bytes:self.outstanding,peak_outstanding_bytes:self.peak,
        native_retained_bytes:self.ledger.len() * 64, reserved_bytes:self.outstanding,
        terminal:self.terminal.clone(), generated_rows:self.generated_rows,rejected_acks:self.rejected_acks,
        monotonic_micros: START.get_or_init(Instant::now).elapsed().as_micros() as u64,
    } }
}
#[derive(Serialize)]
#[serde(rename_all="camelCase")]
struct Frame { status:Status,sequence:u64,encoded_bytes:usize,payload:Option<String>,digest:Option<u32> }
fn hash(bytes:&[u8]) -> u32 { bytes.iter().fold(0x811c9dc5u32,|value,byte| (value ^ u32::from(*byte)).wrapping_mul(0x01000193)) }
fn cell(row:u32,column:u32,seed:u64) -> Value {
    match column % 8 {
        0 => json!({"kind":"null"}),
        1 => json!({"kind":"text","value":""}),
        2 => json!({"kind":"u64","value":"18446744073709551615"}),
        3 => json!({"kind":"decimal","value":format!("{row}.12345678901234567890")}),
        4 => json!({"kind":"text","value":format!("한글 행 {row} 열 {column} 시드 {seed}")}),
        5 => json!({"kind":"bytes","base64":"AAECA//+"}),
        6 => json!({"kind":"temporal","value":"2026-11-17T00:00:00.123456+09:00","precision":6}),
        _ => json!({"kind":"i64","value":format!("-{}",u64::from(row)*100+u64::from(column))}),
    }
}
#[tauri::command]
fn bench_open(state:tauri::State<'_,Mutex<Producer>>,seed:u64,mode:String) -> Result<Status,String> {
    if !["normal","slow","no_ack","cancel"].contains(&mode.as_str()) { return Err("INVALID_MODE".into()); }
    let mut producer=state.lock().map_err(|_|"POISONED")?;
    let generation=producer.generation.checked_add(1).ok_or("GENERATION_OVERFLOW")?;
    *producer=Producer { generation,seed,terminal:"streaming".into(),..Producer::default() };
    Ok(producer.status())
}
#[tauri::command]
fn bench_next(state:tauri::State<'_,Mutex<Producer>>,generation:u64,start_row:u32,row_count:u32,oversize:String) -> Result<Frame,String> {
    let mut producer=state.lock().map_err(|_|"POISONED")?;
    producer.check(generation)?; producer.expire();
    if oversize != "none" {
        if !["cell","frame"].contains(&oversize.as_str()) { return Err("INVALID_OVERSIZE_CASE".into()); }
        // Declared oversized input is refused before constructing its body.
        producer.terminal="oversize".into();producer.ledger.clear();producer.outstanding=0;
        return Ok(Frame {status:producer.status(),sequence:producer.sequence,encoded_bytes:0,payload:None,digest:None});
    }
    if row_count==0 || row_count>28 || start_row>=ROWS || start_row.checked_add(row_count).is_none_or(|end| end>ROWS) { return Err("ROW_LIMIT".into()); }
    if producer.terminal!="streaming" || producer.outstanding.checked_add(FRAME).is_none_or(|size| size>CREDIT) {
        return Ok(Frame {status:producer.status(),sequence:producer.sequence,encoded_bytes:0,payload:None,digest:None});
    }
    // Charge the worst-case frame BEFORE generation/serialization, then return unused credit.
    producer.outstanding += FRAME;
    let payload:Vec<Vec<Value>>=(start_row..start_row+row_count).map(|row| (0..COLUMNS).map(|column| cell(row,column,producer.seed)).collect()).collect();
    let text=serde_json::to_string(&payload).map_err(|_|"ENCODING")?;
    let payload_value=json!({"payload":text,"sequence":producer.sequence+1});
    let encoded=serde_json::to_vec(&payload_value).map_err(|_|"ENCODING")?.len()+1024;
    if encoded>FRAME {
        producer.outstanding-=FRAME;producer.terminal="oversize".into();
        return Ok(Frame {status:producer.status(),sequence:producer.sequence,encoded_bytes:0,payload:None,digest:None});
    }
    producer.outstanding-=FRAME-encoded;
    producer.sequence+=1;let sequence=producer.sequence;
    producer.ledger.insert(sequence,encoded);producer.peak=producer.peak.max(producer.outstanding);
    producer.generated_rows+=u64::from(row_count);
    let digest=hash(text.as_bytes());
    Ok(Frame {status:producer.status(),sequence,encoded_bytes:encoded,payload:Some(text),digest:Some(digest)})
}
#[tauri::command]
fn bench_ack(state:tauri::State<'_,Mutex<Producer>>,generation:u64,sequence:u64) -> Result<Status,String> {
    let mut producer=state.lock().map_err(|_|"POISONED")?;
    producer.check(generation)?;producer.expire();
    if sequence>producer.sequence { producer.rejected_acks+=1;return Err("ACK_FUTURE".into()); }
    if sequence<=producer.acknowledged || producer.terminal!="streaming" { return Ok(producer.status()); }
    let keys:Vec<u64>=producer.ledger.range(..=sequence).map(|(key,_)|*key).collect();
    for key in keys { if let Some(bytes)=producer.ledger.remove(&key) { producer.outstanding=producer.outstanding.checked_sub(bytes).ok_or("CREDIT_UNDERFLOW")?; } }
    producer.acknowledged=sequence;producer.last_ack=Instant::now();Ok(producer.status())
}
#[tauri::command]
fn bench_status(state:tauri::State<'_,Mutex<Producer>>,generation:u64) -> Result<Status,String> {
    let mut producer=state.lock().map_err(|_|"POISONED")?;producer.check(generation)?;producer.expire();Ok(producer.status())
}
#[tauri::command]
fn bench_cancel(state:tauri::State<'_,Mutex<Producer>>,generation:u64) -> Result<Status,String> {
    let mut producer=state.lock().map_err(|_|"POISONED")?;producer.check(generation)?;
    producer.terminal="cancelled".into();producer.ledger.clear();producer.outstanding=0;Ok(producer.status())
}
#[tauri::command]
fn bench_info() -> Value { json!({"producer":"Rust lazy rows; no DB/JS producer","rows":ROWS,"columns":COLUMNS,"frameLimit":FRAME,"creditLimit":CREDIT,"hash":"FNV1a32 over exact UTF8 payload; corruption check not cryptography","nativeRetainedCounter":"ledger metadata only; transient JSON Vec/text/IPC copies measured by process-tree working set","monotonicMicros":START.get_or_init(Instant::now).elapsed().as_micros() as u64}) }
#[tauri::command]
fn bench_exit(app:tauri::AppHandle) { app.exit(0) }
fn main() {
    tauri::Builder::default().manage(Mutex::new(Producer::default()))
        .invoke_handler(tauri::generate_handler![bench_open,bench_next,bench_ack,bench_status,bench_cancel,bench_info,bench_exit])
        .run(tauri::generate_context!()).expect("isolated benchmark startup");
}
'@
$taskBundleScript = Join-Path $taskOutput 'bundle.cjs'
Write-TaskFile $taskBundleScript @'
const path = require('node:path');
const root=process.argv[2], out=process.argv[3];
require(path.join(root,'apps/console/node_modules/esbuild')).buildSync({entryPoints:[path.join(root,'apps/console/bench/workbench-performance.ts')],bundle:true,outfile:path.join(out,'native-harness/web/bench.js'),platform:'browser',target:'es2021',define:{'process.env.NODE_ENV':'"production"'},logLevel:'info'});
'@
& node $taskBundleScript $taskRepo $taskOutput
if ($LASTEXITCODE -ne 0) { throw 'Benchmark frontend bundle failed.' }
$taskRunner = Join-Path $taskOutput 'drive.cjs'
Write-TaskFile $taskRunner @'
const fs=require('node:fs'),path=require('node:path');
const root=process.argv[2],out=process.argv[3],port=process.argv[4];
const {chromium}=require(path.join(root,'apps/console/node_modules/playwright'));
const sleep=ms=>new Promise(r=>setTimeout(r,ms));
const write=(name,value)=>fs.writeFileSync(path.join(out,name),JSON.stringify(value,null,2));
const protocolEvents=[],heapSamples=[],consoleEvents=[];
const jsonl=(name,values)=>fs.writeFileSync(path.join(out,name),values.map(v=>JSON.stringify(v)).join('\n')+'\n');
(async()=>{
 let browser;
 for(let attempt=0;attempt<120;attempt++){try{browser=await chromium.connectOverCDP(`http://127.0.0.1:${port}`);break;}catch{await sleep(500);}}
 if(!browser)throw Error('ACTUAL_WEBVIEW_CDP_UNAVAILABLE');
 let page;
 for(let attempt=0;attempt<60;attempt++){
   page=browser.contexts().flatMap(c=>c.pages()).find(p=>/tauri|localhost/.test(p.url()));
   if(page)break;await sleep(500);
 }
 if(!page)throw Error('WEBVIEW_PAGE_MISSING');
 page.setDefaultTimeout(30000);
 await page.waitForFunction(()=>window.workbenchBenchmark?.ready);
 const session=await page.context().newCDPSession(page);
 page.on('console',message=>{if(consoleEvents.length<200)consoleEvents.push({type:message.type(),text:message.text()});});
 session.on('Network.requestWillBeSent',event=>{if(event.request.url.includes('ipc.localhost'))protocolEvents.push({kind:'request',at:event.timestamp,url:event.request.url});});
 session.on('Network.responseReceived',event=>{if(event.response.url.includes('ipc.localhost'))protocolEvents.push({kind:'response',at:event.timestamp,url:event.response.url,status:event.response.status,mime:event.response.mimeType});});
 await session.send('Network.enable',{maxTotalBufferSize:1048576,maxResourceBufferSize:65536});
 await session.send('Performance.enable');
 write('webview.json',{url:page.url(),version:await session.send('Browser.getVersion'),display:await page.evaluate(()=>({width:innerWidth,height:innerHeight,dpr:devicePixelRatio,userAgent:navigator.userAgent}))});
 write('dataset.json',await page.evaluate(()=>window.workbenchBenchmark.manifest));
 await page.screenshot({path:path.join(out,'surface.png')});
 write('phase.json',{phase:'idle',atMs:Date.now()});await sleep(2000);
 for(let run=0;run<=5;run++){
   write('phase.json',{phase:run===0?'warmup':`measured:${run}`,atMs:Date.now()});
   await page.evaluate(()=>window.workbenchBenchmark.beginRun());
   for(let index=0;index<200;index++){
     const action=index%20===19?'cancel':index%2===0?'keyboard':'scroll';
     await page.evaluate(({run,index,action})=>window.workbenchBenchmark.arm(run,index,action),{run,index,action});
     if(action==='scroll')await session.send('Input.dispatchMouseEvent',{type:'mouseWheel',x:500,y:300,deltaX:0,deltaY:120});
     else await session.send('Input.dispatchKeyEvent',{type:'keyDown',key:action==='cancel'?'Escape':'ArrowDown',code:action==='cancel'?'Escape':'ArrowDown',windowsVirtualKeyCode:action==='cancel'?27:40});
     await page.evaluate(()=>window.workbenchBenchmark.settled());
     if(action!=='scroll')await session.send('Input.dispatchKeyEvent',{type:'keyUp',key:action==='cancel'?'Escape':'ArrowDown',code:action==='cancel'?'Escape':'ArrowDown',windowsVirtualKeyCode:action==='cancel'?27:40});
   }
   const snapshot=await page.evaluate(()=>window.workbenchBenchmark.snapshot());
   heapSamples.push({run,atMs:Date.now(),heap:await session.send('Runtime.getHeapUsage'),dom:await session.send('Memory.getDOMCounters'),performance:await session.send('Performance.getMetrics')});
   write(`run-${run}.json`,{inputs:snapshot.inputs.filter(x=>x.run===run),counters:snapshot.counters.filter(x=>x.phase===`input:${run}`)});
 }
 write('phase.json',{phase:'faults',atMs:Date.now()});
 const faults=await page.evaluate(()=>window.workbenchBenchmark.faults());write('faults.json',faults);
 const metrics=await page.evaluate(()=>window.workbenchBenchmark.finish());
 const raw=await page.evaluate(()=>window.workbenchBenchmark.snapshot());
 jsonl('raw-input.jsonl',raw.inputs);jsonl('frames.jsonl',raw.frames.map((durationMs,index)=>({index,durationMs})));jsonl('counters.jsonl',raw.counters);write('errors.json',raw.errors);jsonl('protocol.jsonl',protocolEvents);write('heap.json',heapSamples);write('console.json',consoleEvents);
 const violations=raw.counters.filter(c=>c.status.outstandingBytes>8*1024*1024||c.retainedBytes>32*1024*1024||c.status.nativeRetainedBytes>64*1024*1024||c.mountedCells>336).length;
 const M3=violations+faults.filter(f=>!f.passed).length+raw.inputs.filter(x=>!x.digestVerified||!x.trustedEvent).length+raw.errors.length;
 write('frontend-metrics.json',{...metrics,M3,measuredSamples:raw.inputs.filter(x=>x.run>0).length,trustedInputSamples:raw.inputs.filter(x=>x.trustedEvent).length,allFaultsPassed:faults.every(f=>f.passed),boundaryScope:'synthetic credit/type/terminal only; real permissions and commits NOT RUN'});
 write('phase.json',{phase:'complete',atMs:Date.now()});
 await page.evaluate(()=>window.__TAURI__.core.invoke('bench_exit')).catch(()=>{});await browser.close().catch(()=>{});
 const exitCode=M3!==0||!metrics.negativeChecks.missingRejected||!metrics.negativeChecks.overflowRejected?1:0;
 write('driver-exit.json',{exitCode,completedAtMs:Date.now()});process.exitCode=exitCode;
})().catch(error=>{write('runner-error.json',{error:String(error),stack:error.stack});write('driver-exit.json',{exitCode:1,completedAtMs:Date.now()});console.error(error);process.exitCode=1;});
'@
}
$taskBinary = Join-Path $taskRepo 'target\debug\restoreplane-workbench-bench.exe'
$taskBuildPrefix = if ($ResumeBuild) { 'build-resumed' } else { 'build' }
$taskCargoArguments = @('build','--manifest-path',('"' + (Join-Path $taskHarness 'Cargo.toml') + '"'),'--target-dir',('"' + (Join-Path $taskRepo 'target') + '"'))
$taskCargoProcess = Start-Process -FilePath (Get-Command cargo).Source -ArgumentList $taskCargoArguments -WindowStyle Hidden -PassThru -RedirectStandardOutput (Join-Path $taskOutput ($taskBuildPrefix + '.stdout.log')) -RedirectStandardError (Join-Path $taskOutput ($taskBuildPrefix + '.stderr.log'))
$taskCargoHandle = $taskCargoProcess.Handle
while (-not $taskCargoProcess.HasExited) { Start-Sleep -Milliseconds 500; $taskCargoProcess.Refresh() }
$taskCargoProcess.WaitForExit()
$taskBuildExit = $taskCargoProcess.ExitCode
if ($null -eq $taskBuildExit -or $taskBuildExit -ne 0) { Write-TaskJson (Join-Path $taskOutput 'summary.json') @{status='NOT_RUN';reason='native build failed or OS exit unavailable';exitCode=$taskBuildExit}; throw 'Native benchmark build failed; see separate stdout/stderr logs.' }
if (-not (Test-Path -LiteralPath $taskBinary)) { throw 'Native executable missing.' }
$taskBuiltEnvironment = Get-Content -Raw -LiteralPath (Join-Path $taskOutput 'environment.json') | ConvertFrom-Json
$taskBuiltEnvironment | Add-Member -NotePropertyName executableSha256 -NotePropertyValue (Get-FileHash -LiteralPath $taskBinary -Algorithm SHA256).Hash.ToLowerInvariant() -Force
Write-TaskJson (Join-Path $taskOutput 'environment.json') $taskBuiltEnvironment
$taskOldBrowserArgs = $env:WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS
$taskOldUserFolder = $env:WEBVIEW2_USER_DATA_FOLDER
$taskAppProcess = $null; $taskDriverProcess = $null
$taskMemory = New-Object 'System.Collections.Generic.List[object]'
try {
    $env:WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS = "--remote-debugging-port=$DebugPort --remote-allow-origins=* --disable-background-timer-throttling --disable-renderer-backgrounding"
    $env:WEBVIEW2_USER_DATA_FOLDER = Join-Path $taskOutput 'webview-user-data'
    $taskAppProcess = Start-Process -FilePath $taskBinary -WorkingDirectory $taskHarness -WindowStyle Hidden -PassThru -RedirectStandardOutput (Join-Path $taskOutput 'native.stdout.log') -RedirectStandardError (Join-Path $taskOutput 'native.stderr.log')
    $taskDriverArgs = @('"' + $taskRunner + '"','"' + $taskRepo + '"','"' + $taskOutput + '"',[string]$DebugPort)
    $taskDriverProcess = Start-Process -FilePath (Get-Command node).Source -ArgumentList $taskDriverArgs -WindowStyle Hidden -PassThru -RedirectStandardOutput (Join-Path $taskOutput 'driver.stdout.log') -RedirectStandardError (Join-Path $taskOutput 'driver.stderr.log')
    $taskDriverHandle = $taskDriverProcess.Handle
    $taskTimeout = [DateTime]::UtcNow.AddMinutes(6)
    while (-not $taskDriverProcess.HasExited -and [DateTime]::UtcNow -lt $taskTimeout) {
        $taskProcesses = @(Get-CimInstance Win32_Process | Select-Object ProcessId,ParentProcessId)
        $taskIds = New-Object 'System.Collections.Generic.HashSet[int]'; [void]$taskIds.Add($taskAppProcess.Id)
        do { $taskAdded = $false; foreach ($taskProcess in $taskProcesses) { if ($taskIds.Contains([int]$taskProcess.ParentProcessId) -and $taskIds.Add([int]$taskProcess.ProcessId)) { $taskAdded = $true } } } while ($taskAdded)
        [long]$taskWorkingSet = 0; $taskPidSamples = @()
        foreach ($taskProcessId in $taskIds) {
            $taskObserved = Get-Process -Id $taskProcessId -ErrorAction SilentlyContinue
            if ($taskObserved) { $taskWorkingSet += $taskObserved.WorkingSet64; $taskPidSamples += "$taskProcessId`:$($taskObserved.WorkingSet64)" }
        }
        $taskPhase = 'launch'
        $taskPhasePath = Join-Path $taskOutput 'phase.json'
        if (Test-Path -LiteralPath $taskPhasePath) { try { $taskPhase = (Get-Content -Raw -LiteralPath $taskPhasePath | ConvertFrom-Json).phase } catch {} }
        $taskMemory.Add([pscustomobject]@{atMs=[DateTimeOffset]::UtcNow.ToUnixTimeMilliseconds();phase=$taskPhase;workingSetBytes=$taskWorkingSet;processCount=$taskIds.Count;pids=($taskPidSamples -join '|')})
        Start-Sleep -Milliseconds 250
        $taskDriverProcess.Refresh()
    }
    if (-not $taskDriverProcess.HasExited) { Stop-Process -Id $taskDriverProcess.Id -ErrorAction SilentlyContinue; throw 'Actual WebView benchmark timed out; partial raw evidence retained.' }
    $taskDriverProcess.WaitForExit()
    $taskDriverExit = $taskDriverProcess.ExitCode
    if ($null -eq $taskDriverExit) { throw 'DATA NONE: OS driver exit code unavailable despite retained handle.' }
    $taskMemory | Export-Csv -LiteralPath (Join-Path $taskOutput 'memory.csv') -NoTypeInformation -Encoding UTF8
    $taskMetricsPath = Join-Path $taskOutput 'frontend-metrics.json'
    if (-not (Test-Path -LiteralPath $taskMetricsPath)) { throw 'DATA NONE: native/WebView driver did not produce complete metrics; inspect runner-error.json.' }
    $taskMetrics = Get-Content -Raw -LiteralPath $taskMetricsPath | ConvertFrom-Json
    $taskIdle = @($taskMemory | Where-Object phase -eq 'idle' | Select-Object -ExpandProperty workingSetBytes)
    if ($taskIdle.Count -eq 0) { throw 'DATA NONE: idle process-tree baseline missing.' }
    [double]$taskBaseline = ($taskIdle | Measure-Object -Average).Average
    [double]$taskPeak = ($taskMemory | Measure-Object -Property workingSetBytes -Maximum).Maximum
    $taskM2 = ($taskPeak - $taskBaseline) / 1MB
    $taskM1Decision = if ($taskMetrics.inputP95Ms -le 100) {'continue'} elseif ($taskMetrics.inputP95Ms -le 150) {'early_signal'} elseif ($taskMetrics.inputP95Ms -le 250) {'adjust_review'} else {'stop_review'}
    $taskM2Decision = if ($taskM2 -le 256) {'continue'} elseif ($taskM2 -le 384) {'early_signal'} elseif ($taskM2 -le 512) {'adjust_review'} else {'stop_review'}
    $taskHashes = @(Get-ChildItem -LiteralPath $taskOutput -File -Recurse | Where-Object { $_.FullName -notlike '*webview-user-data*' -and $_.Extension -ne '.log' -and $_.Name -ne 'Cargo.lock' } | ForEach-Object { @{path=$_.FullName.Substring($taskOutput.Length+1);sha256=(Get-FileHash -Algorithm SHA256 -LiteralPath $_.FullName).Hash.ToLowerInvariant()} })
    $taskSummary = [ordered]@{status=if ($taskDriverExit -eq 0) {'measured'} else {'failed'};runId=$RunId;M1InputP95Ms=$taskMetrics.inputP95Ms;M2DeltaWorkingSetMiB=$taskM2;M3SyntheticBoundaryViolations=$taskMetrics.M3;idleWorkingSetBytes=$taskBaseline;peakWorkingSetBytes=$taskPeak;M1Decision=$taskM1Decision;M2Decision=$taskM2Decision;M3Decision=if ($taskMetrics.M3 -eq 0) {'continue'} else {'stop_review'};sampleCount=$taskMetrics.measuredSamples;frameP95Ms=$taskMetrics.frameP95Ms;driverExit=$taskDriverExit;faultsPassed=$taskMetrics.allFaultsPassed;artifacts=$taskHashes;limitations=@('Synthetic producer; live DB/authorization/commit faults NOT RUN','Double rAF observation is not physical display photometry','Shared working sets can be counted more than once','Production build and shared Console composition unchanged')}
    Write-TaskJson (Join-Path $taskOutput 'summary.json') $taskSummary
    [pscustomobject]$taskSummary | Select-Object status,runId,M1InputP95Ms,M2DeltaWorkingSetMiB,M3SyntheticBoundaryViolations,M1Decision,M2Decision,driverExit | Format-List
    if ($taskDriverExit -ne 0) { throw 'Benchmark scenario failed; first evidence is preserved.' }
}
catch {
    if (-not (Test-Path -LiteralPath (Join-Path $taskOutput 'summary.json'))) {
        Write-TaskJson (Join-Path $taskOutput 'summary.json') @{status='NOT_RUN';reason=$_.Exception.Message;M1='DATA_NONE';M2='DATA_NONE';M3='DATA_NONE'}
    }
    throw
}
finally {
    if ($taskAppProcess) { $taskAppProcess.Refresh(); if (-not $taskAppProcess.HasExited) { Stop-Process -Id $taskAppProcess.Id -ErrorAction SilentlyContinue } }
    if ($taskMemory.Count -gt 0 -and -not (Test-Path -LiteralPath (Join-Path $taskOutput 'memory.csv'))) { $taskMemory | Export-Csv -LiteralPath (Join-Path $taskOutput 'memory.csv') -NoTypeInformation -Encoding UTF8 }
    $env:WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS = $taskOldBrowserArgs
    $env:WEBVIEW2_USER_DATA_FOLDER = $taskOldUserFolder
}
