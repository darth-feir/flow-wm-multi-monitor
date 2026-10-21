#Requires AutoHotkey v2.0
#SingleInstance Force
#NoTrayIcon
; ════════════════════════════════════════════════════════════════════════════
; FlowWM resident starter. Runs all the time (put a shortcut to it in
; shell:startup) and owns exactly one chord:
;
;   Mod+Shift+E   start FlowWM (`flow start --ahk`) when no daemon is running
;
; While the daemon runs, flow.ahk owns the same chord and stops FlowWM, so
; Mod+Shift+E is a plain on/off switch. Both scripts hook the key; the one
; whose #HotIf condition is false lets it pass through to the other, and the
; conditions are exclusive (daemon pipe absent / present).
;
; flow.exe is found via the FLOW_BIN environment variable (a directory), then
; %LOCALAPPDATA%\Programs\flow, then PATH. Its directory is put on PATH before
; the start so the daemon's keymap (which runs plain `flow`) inherits it.
; ════════════════════════════════════════════════════════════════════════════

PipeName := EnvGet("FLOW_PIPE_NAME") || "\\.\pipe\flow"
FlowExe := ResolveFlowExe()

ResolveFlowExe() {
    dir := EnvGet("FLOW_BIN")
    if (dir != "" && FileExist(dir "\flow.exe"))
        return dir "\flow.exe"
    perUser := EnvGet("LOCALAPPDATA") "\Programs\flow\flow.exe"
    if FileExist(perUser)
        return perUser
    return "flow"
}

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
    if (FlowExe != "flow") {
        SplitPath(FlowExe, , &dir)
        EnvSet("Path", dir ";" EnvGet("Path"))
    }
    Note("Starting FlowWM…")
    try Run('"' FlowExe '" start --ahk', , "Hide")
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
#+e::StartFlow()
#HotIf
