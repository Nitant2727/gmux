# Emits, to its own stdout, plain-text markers interleaved with OSC 9 / 777 / 99 notification
# sequences. Run INSIDE a gmux ConPTY pane; the host reads this back and asserts the OSC
# sequences arrived intact and in the correct order relative to the markers.
# All three OSC use the BEL terminator (0x07), which every target parser accepts.

$e   = [char]27          # ESC
$bel = [char]7           # BEL
$out = [Console]::Out

# Diagnostic: where am I attached? A 120-wide window means I'm on gmux's pseudoconsole.
try {
    $diag = Join-Path $PSScriptRoot 'emit_diag.txt'
    "WindowSize=$([Console]::WindowWidth)x$([Console]::WindowHeight); BufferWidth=$([Console]::BufferWidth)" |
        Set-Content -Path $diag -Encoding ascii
} catch {
    "diag-failed: $_" | Set-Content -Path (Join-Path $PSScriptRoot 'emit_diag.txt') -Encoding ascii
}

$out.Write('[[BEGIN]]')
$out.Write("$e]9;gmux osc9 message$bel")
$out.Write('[[M1]]')
$out.Write("$e]777;notify;gmux osc777 title;osc777 body$bel")
$out.Write('[[M2]]')
$out.Write("$e]99;i=1:p=title;gmux osc99$bel")
$out.Write('[[END]]')
$out.Write("`r`n")
$out.Flush()
# Give ConPTY time to re-render our output to the host before we exit and it tears down.
Start-Sleep -Milliseconds 250
