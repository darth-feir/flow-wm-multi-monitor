; ════════════════════════════════════════════════════════════════════════════
; Where flow.exe is. Shared by flow.ahk and flow-resident.ahk (#Include);
; keep it beside them in %USERPROFILE%\.config\flow.
;
; Order: %FLOW_BIN%\flow.exe (set by the daemon for the keymap it launches),
; then %LOCALAPPDATA%\Programs\flow\flow.exe, then plain `flow` from PATH.
; Never the working directory (see README.md).
; ════════════════════════════════════════════════════════════════════════════

ResolveFlowExe() {
    dir := EnvGet("FLOW_BIN")
    if (dir != "" && FileExist(dir "\flow.exe"))
        return dir "\flow.exe"
    perUser := EnvGet("LOCALAPPDATA") "\Programs\flow\flow.exe"
    if FileExist(perUser)
        return perUser
    return "flow"
}
