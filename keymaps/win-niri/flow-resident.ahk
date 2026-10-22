#Requires AutoHotkey v2.0
#SingleInstance Force
#NoTrayIcon
; ════════════════════════════════════════════════════════════════════════════
; FlowWM resident starter. Runs all the time (put a shortcut to it in
; shell:startup) and owns exactly one chord:
;
;   Mod+Ctrl+T    start FlowWM (`flow start --ahk`) when no daemon is running
;
; While the daemon runs, flow.ahk owns the same chord and stops FlowWM, so
; Mod+Ctrl+T is a plain on/off switch. Both scripts hook the key; the one
; whose #HotIf condition is false lets it pass through to the other, and the
; conditions are exclusive (daemon pipe absent / present).
;
; flow.exe is resolved by flow-exe.ahk (shared with flow.ahk): the FLOW_BIN
; environment variable (a directory), then %LOCALAPPDATA%\Programs\flow, then
; PATH. The daemon sets FLOW_BIN for the keymap it launches itself.
; ════════════════════════════════════════════════════════════════════════════

#Include %A_ScriptDir%\flow-exe.ahk

PipeName := EnvGet("FLOW_PIPE_NAME") || "\\.\pipe\flow"
FlowExe := ResolveFlowExe()

; True when nothing answers on the daemon's pipe. ERROR_SEM_TIMEOUT (121)
; means the pipe exists but is busy, i.e. a daemon is there.
DaemonDown() {
    global PipeName
    if DllCall("WaitNamedPipe", "Str", PipeName, "UInt", 1)
        return false
    return A_LastError != 121
}

Note(msg, ms := 2500) {
    ToolTip(msg)
    SetTimer(() => ToolTip(), -ms)
}

StartFlow() {
    global FlowExe
    Note("Starting FlowWM…")
    ; Started from the home folder: the daemon must not inherit this script's
    ; working directory either (it passes its own to the keymap it launches).
    try Run('"' FlowExe '" start --ahk', EnvGet("USERPROFILE"), "Hide")
    catch as e {
        Note("could not run flow start: " e.Message, 5000)
        return
    }
    SetTimer(ReportStart, -8000)
}

ReportStart() {
    if DaemonDown()
        Note("FlowWM did not start; see the log in %USERPROFILE%\.config\flow", 5000)
}

#HotIf DaemonDown()
#^t::StartFlow()
#HotIf
