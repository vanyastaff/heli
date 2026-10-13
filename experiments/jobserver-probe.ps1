param([string]$Probe, [int]$Phase1Seconds = 25)
# Proves that cargo on Windows draws from a named semaphore given in
# CARGO_MAKEFLAGS: with 0 tokens it may run only its one implicit job, after
# releasing 5 tokens at most 6, and without a jobserver as many as it likes.

function Max-Rustc([System.Diagnostics.Process]$cargo, [double]$seconds) {
    $max = 0; $deadline = (Get-Date).AddSeconds($seconds)
    while ((Get-Date) -lt $deadline -and -not $cargo.HasExited) {
        $n = @(Get-CimInstance Win32_Process -Filter "Name='rustc.exe'" |
            Where-Object { $_.CommandLine -like "*jobserver-probe*target*" }).Count
        if ($n -gt $max) { $max = $n }
        Start-Sleep -Milliseconds 150
    }
    $max
}

function Start-Build([string]$makeflags) {
    Remove-Item -Recurse -Force "$Probe\target" -ErrorAction SilentlyContinue
    if ($makeflags) { $env:CARGO_MAKEFLAGS = $makeflags } else { Remove-Item Env:CARGO_MAKEFLAGS -ErrorAction SilentlyContinue }
    $env:CARGO_TARGET_DIR = "$Probe\target"
    Start-Process cargo -ArgumentList 'build', '--offline', '--quiet', '--manifest-path', "$Probe\Cargo.toml" `
        -NoNewWindow -PassThru -RedirectStandardError "$Probe\build-$([guid]::NewGuid()).log"
}

$name = "heli-probe-$PID"
$sem = [System.Threading.Semaphore]::new(0, 5, $name)   # 0 free tokens, 5 max

$cargo = Start-Build "-j --jobserver-auth=$name"
$phase1 = Max-Rustc $cargo $Phase1Seconds
[void]$sem.Release(5)
$phase2 = Max-Rustc $cargo 600
$cargo.WaitForExit()
"with jobserver, 0 tokens free : max concurrent rustc = $phase1 (expect 1)"
"after releasing 5 tokens      : max concurrent rustc = $phase2 (expect <= 6)"
"build exit code               : $($cargo.ExitCode)"

$control = Start-Build $null
$free = Max-Rustc $control 600
$control.WaitForExit()
"control, no jobserver         : max concurrent rustc = $free"
$sem.Dispose()
