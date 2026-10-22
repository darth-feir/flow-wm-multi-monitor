; ════════════════════════════════════════════════════════════════════════════
; Where flow.exe is. Shared by flow.ahk and flow-resident.ahk (#Include) so
; both resolve it the same way; keep it beside them in %USERPROFILE%\.config\flow.
;
; Order:
;   1. %FLOW_BIN%\flow.exe   - FLOW_BIN is the directory of the running
;                              flowd.exe; the daemon sets it in the keymap's
;                              environment when it launches flow.ahk (so the
;                              keymap always drives the daemon it belongs to).
;                              Set it yourself for flow-resident.ahk if flow
;                              is installed somewhere unusual.
;   2. %LOCALAPPDATA%\Programs\flow\flow.exe   - the usual install location.
;   3. "flow"                - last resort: PATH. Never the working directory
;                              on purpose: CreateProcess searches the cwd
;                              before PATH, and the daemon used to inherit
;                              whatever cwd `flow start` was run from.
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
