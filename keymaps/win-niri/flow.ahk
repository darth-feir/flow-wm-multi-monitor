#Requires AutoHotkey v2.0
#SingleInstance Force
; ════════════════════════════════════════════════════════════════════════════
; FlowWM keybindings, niri flavour, Windows key as Mod, arrow keys.
;
; This script is an interpreter: the chords come from the daemon. At start
; (and again whenever flow.toml is saved) it runs `flow query binds --plain`,
; which lists the `[binds]` table of flow.toml laid over the keymap's
; built-in table (binds.toml next to this file), and registers every chord
; with Hotkey(). The help overlay (help.ahk) is built from the same listing.
; Install at %USERPROFILE%\.config\flow\flow.ahk (together with flow-exe.ahk,
; selection.ahk, launcher.ahk and help.ahk, which it #Includes) and start with
; `flow start --ahk`. Design notes, the daemon-liveness gate, the tiling
; toggle, which Windows keys are shadowed and the AutoHotkey lessons are in
; README.md next to this file; the bind grammar and the action table are in
; the user guide (Config Reference, `[binds]`).
;
; Hardwired here, not in the table:
;   Mod + left-button drag        move a window grabbed anywhere (a tile drops into the zone under the cursor, a float
;                                 moves; a click that moves less than 8 px only activates the window)
;   Mod+Ctrl+T                    stop FlowWM fully (daemon and this script); flow-resident.ahk,
;                                 if it runs, starts FlowWM again with the same chord
;   Mod+Ctrl+Shift+T              pause toggle: `flow dispatch tiling toggle` (daemon keeps running)
;   middle click                  paste the selection buffer, only with `[keymap] selection_buffer = true`
;                                 (selection.ahk; README.md *Selection buffer*)
;
; Every chord is bound only while the daemon's pipe answers; window-management
; chords only while tiling is on; Win+L stays Windows.
; ════════════════════════════════════════════════════════════════════════════

#Include %A_ScriptDir%\flow-exe.ahk
; Included before the binds are applied: ApplyBinds() switches it on or off.
#Include %A_ScriptDir%\selection.ahk

flowExe := ResolveFlowExe()   ; absolute path to flow.exe (see flow-exe.ahk); quote-safe below
PipeName := EnvGet("FLOW_PIPE_NAME") || "\\.\pipe\flow"
DaemonUp := false
TilingOn := true        ; refined from `flow ping` after the startup gate
DownTicks := 0
BusyPolls := 0          ; consecutive polls that found the pipe busy (121)
MaxBusyPolls := 5       ; after this many, "busy" means wedged, not alive
CfgPath := (EnvGet("FLOW_CONFIG_DIR") || A_ScriptDir) "\flow.toml"
CfgSeen := ""           ; flow.toml's modification time at the last look
CfgChangedAt := 0       ; A_TickCount when a change was first seen (0: nothing pending)
BindsText := ""         ; the listing the current hotkeys were registered from
Registered := []        ; [hotkey, scope] of every chord registered from it
HelpRows := []          ; [chords, help] rows for help.ahk
HelpGui := 0            ; the overlay panel, built by help.ahk's BuildHelpGui() from HelpRows (declared here:
                        ; ApplyBinds() runs before the include's own auto-execute lines)

Flow(cmd) {
    global flowExe
    try Run('"' flowExe '" ' cmd, , 'Hide')
    catch as e
        Osd("could not run flow: " e.Message)
}

; Dispatch without blocking the hotkey thread (Run returns at once); the exit
; code is collected asynchronously by DispatchDone so a failed dispatch still
; re-checks the daemon right away and the next chord falls through to Windows.
; Set FLOW_AHK_LOG=<path> in the environment to trace every dispatch and its exit code.
AhkLog(msg) {
    static path := EnvGet("FLOW_AHK_LOG")
    if path
        try FileAppend(FormatTime(, "HH:mm:ss") "." A_MSec " " msg "`n", path)
}

; Short on-screen message in the bottom-right corner of the primary screen,
; gone after `ms`. Never TrayTip: on Windows 11 the toast holds the
; foreground lock until dismissed (see README.md).
; The static must not be called `gui`: a local of that name shadows the Gui
; class (names are case-insensitive) and `Gui(...)` then calls an Integer.
Osd(msg, ms := 2500) {
    static box := 0
    AhkLog("osd: " msg)
    if box
        try box.Destroy()
    box := Gui("+AlwaysOnTop -Caption +ToolWindow +Owner +E0x20", "FlowWM")   ; E0x20: click-through
    box.BackColor := 0x1E1E1E
    box.MarginX := 16, box.MarginY := 10
    box.SetFont("s11 cE8E8E8", "Segoe UI")
    box.AddText("w520", msg)
    MonitorGetWorkArea(MonitorGetPrimary(), &l, &t, &r, &b)
    box.Show("NoActivate Hide")
    box.GetPos(, , &w, &h)
    box.Show("NoActivate x" (r - w - 24) " y" (b - h - 24))
    SetTimer(OsdHide.Bind(box), -ms)
}
OsdHide(box) {
    try box.Hide()
}

; onExit (optional): a function object called as onExit(exitCode) once the
; dispatch has finished (not called if the process could not be opened or is
; still running after ~5 s).
D(cmd, onExit := 0) {
    global flowExe
    AhkLog("dispatch " cmd)
    try Run('"' flowExe '" dispatch ' cmd, , 'Hide', &pid)
    catch as e {
        Osd("could not run flow: " e.Message)
        return
    }
    ; PROCESS_QUERY_LIMITED_INFORMATION | SYNCHRONIZE: enough to read the exit code.
    h := DllCall("OpenProcess", "UInt", 0x1000 | 0x100000, "Int", 0, "UInt", pid, "Ptr")
    if h
        SetTimer(DispatchDone.Bind(h, 0, onExit), -50)
}

; Reap one dispatch: poll its process every 50 ms (never blocking a hotkey);
; a non-zero exit means the daemon did not answer, so the liveness check runs
; now instead of at the next 2 s tick. Gives up after ~5 s.
DispatchDone(h, polls, onExit) {
    if (DllCall("WaitForSingleObject", "Ptr", h, "UInt", 0) != 0) {   ; still running
        if (polls < 100)
            SetTimer(DispatchDone.Bind(h, polls + 1, onExit), -50)
        else
            DllCall("CloseHandle", "Ptr", h)
        return
    }
    code := 0
    DllCall("GetExitCodeProcess", "Ptr", h, "UInt*", &code)
    DllCall("CloseHandle", "Ptr", h)
    AhkLog("exit " code " after " polls " poll(s)")
    if (code != 0)
        CheckDaemon()
    if onExit
        onExit(code)
}

; --- Tiling toggle ----------------------------------------------------------
; Ask the daemon whether tiling is on. True unless `flow ping` prints
; "tiling off"; also true if ping cannot be run or does not answer in 3 s.
QueryTiling() {
    global flowExe
    try {
        exec := ComObject("WScript.Shell").Exec('"' flowExe '" ping')
        Loop 60 {
            if (exec.Status != 0)
                break
            Sleep 50
        }
        if (exec.Status = 0) {
            try exec.Terminate()
            AhkLog("ping: no answer in 3 s; assuming tiling on")
            return true
        }
        out := exec.StdOut.ReadAll()
        AhkLog("ping: " Trim(out, " `t`r`n"))
        return !InStr(out, "tiling off")
    } catch as e {
        AhkLog("ping failed: " e.Message)
        return true
    }
}

TilingToggled(code) {
    global TilingOn
    if (code != 0) {
        Osd("tiling toggle failed (flow exit " code ")")
        return
    }
    TilingOn := !TilingOn
    AhkLog("tiling " (TilingOn ? "on" : "off"))
    Osd(TilingOn ? "FlowWM tiling on" : "FlowWM tiling off: Windows is managing windows")
}

; --- Daemon liveness --------------------------------------------------------
DaemonAlive() {
    global PipeName, BusyPolls, MaxBusyPolls
    ; 1 ms wait: nonzero = an instance is free. A_LastError 2 = no such pipe;
    ; 121 (ERROR_SEM_TIMEOUT) = exists but busy, alive only for a bounded run
    ; of polls (a wedged daemon holds the pipe busy forever; see README.md).
    if DllCall("WaitNamedPipe", "Str", PipeName, "UInt", 1) {
        BusyPolls := 0
        return true
    }
    if (A_LastError = 121)
        return ++BusyPolls <= MaxBusyPolls
    BusyPolls := 0
    return false
}

CheckDaemon() {
    global DaemonUp, DownTicks, BusyPolls
    alive := DaemonAlive()
    if (alive != DaemonUp)
        AhkLog("gate: alive=" alive " (was " DaemonUp ") lastError=" A_LastError " busyPolls=" BusyPolls)
    if alive {
        DaemonUp := true, DownTicks := 0
        CheckConfigFile()
        return
    }
    DaemonUp := false            ; chords fall through to Windows immediately
    if (++DownTicks >= 5) {      ; ~10 s on the 2 s timer: release the hook
        Osd("FlowWM daemon is gone; Win key released.")
        Sleep 1500
        ExitApp()
    }
}

; --- Shared GUI helper (used by launcher.ahk and help.ahk) ------------------
; Show a Gui centred on the work area of the monitor that holds the active
; window, then make sure it is active: Windows does not always grant the
; foreground to a window shown from a chord while Win is still held.
ShowCentered(gui) {
    mon := MonitorGetPrimary()
    try {
        if (hwnd := WinExist("A")) {
            WinGetPos(&wx, &wy, &ww, &wh, hwnd)
            cx := wx + ww // 2, cy := wy + wh // 2
            Loop MonitorGetCount() {
                MonitorGet(A_Index, &ml, &mt, &mr, &mb)
                if (cx >= ml && cx < mr && cy >= mt && cy < mb) {
                    mon := A_Index
                    break
                }
            }
        }
    }
    MonitorGetWorkArea(mon, &ml, &mt, &mr, &mb)
    gui.Show("Hide")
    gui.GetPos(, , &gw, &gh)
    gui.Show("x" (ml + (mr - ml - gw) // 2) " y" (mt + (mb - mt - gh) // 2))
    Loop 12 {
        if WinActive("ahk_id " gui.Hwnd)
            return
        try WinActivate("ahk_id " gui.Hwnd)
        Sleep 40
    }
    AhkLog("ShowCentered: could not activate " gui.Title)
}

; --- Programs (the `spawn` binds) -------------------------------------------
; A bare name is looked up the way the daemon's `[startup]` does it (see the
; user guide): Program Files, %LOCALAPPDATA%\Programs, the Store's execution
; aliases, then PATH, never the working directory; a path is taken only when
; absolute. Admin uses the shell's RunAs verb (UAC prompts as usual).
IsFile(p) {
    a := FileExist(p)
    return a != "" && !InStr(a, "D")
}
ResolveProgram(name) {
    static cache := Map()
    if cache.Has(name)
        return cache[name]
    found := ""
    if RegExMatch(name, '[\\/:]') {
        found := RegExMatch(name, '^[A-Za-z]:\\') ? name : ""
    } else {
        stem := RegExReplace(name, 'i)\.exe$')
        for dir in [A_ProgramFiles, EnvGet("ProgramFiles(x86)"), EnvGet("LOCALAPPDATA") "\Programs"]
            if (dir != "" && IsFile(dir "\" stem "\" stem ".exe")) {
                found := dir "\" stem "\" stem ".exe"
                break
            }
        if (found = "" && IsFile(EnvGet("LOCALAPPDATA") "\Microsoft\WindowsApps\" stem ".exe"))
            found := EnvGet("LOCALAPPDATA") "\Microsoft\WindowsApps\" stem ".exe"
        if (found = "") {
            Loop Parse EnvGet("PATH"), ";" {
                dir := Trim(A_LoopField, ' "')
                if (dir = "")
                    continue
                for p in [dir "\" stem ".exe", dir "\" stem]
                    if IsFile(p) {
                        found := p
                        break
                    }
                if (found != "")
                    break
            }
        }
    }
    if (found != "")
        cache[name] := found
    return found
}
Quote(a) {
    return RegExMatch(a, '[\s"]') ? '"' StrReplace(a, '"', '\"') '"' : a
}
Launch(argv, admin := false) {
    exe := ResolveProgram(argv[1])
    if (exe = "") {
        AhkLog("launch: " argv[1] " not found")
        Osd(argv[1] " not found (Program Files, %LOCALAPPDATA%\Programs, the Store's aliases, PATH).")
        return
    }
    args := ""
    Loop argv.Length - 1
        args .= " " Quote(argv[A_Index + 1])
    ; An .exe started without elevation goes through the daemon (`flow spawn`), which asks
    ; its first window to open on the focused screen; running it here is the fallback.
    ; docs/adr/0023-new-windows-open-on-the-focused-screen.md, *Spawning on the focused screen*.
    if (!admin && DaemonUp && RegExMatch(exe, "i)\.exe$") && SpawnViaDaemon(argv, exe, args))
        return
    RunHere(exe, args, admin)
}
; Start exe with its (already quoted) args from the home folder, not wherever the daemon
; (and so this script) was started from.
RunHere(exe, args, admin := false) {
    AhkLog("launch " (admin ? "(admin) " : "") exe args)
    try Run((admin ? "*RunAs " : "") '"' exe '"' args, EnvGet("USERPROFILE"))
    catch as e
        if !InStr(e.Message, "cancel")       ; a declined UAC prompt is not worth a popup
            Osd("could not start " exe ": " e.Message)
}
; `flow spawn -- <exe> <args...>`, each item quoted for the CRT parser so the daemon gets
; them one by one. False when flow.exe could not be run (the caller runs exe itself); a
; non-zero exit later (no daemon, start failed) runs it here too.
SpawnViaDaemon(argv, exe, args) {
    global flowExe
    cmd := "spawn -- " CrtQuote(exe)
    Loop argv.Length - 1
        cmd .= " " CrtQuote(argv[A_Index + 1])
    AhkLog("launch via daemon: " cmd)
    ; This script had the last input, so it may hand the foreground on: to the daemon,
    ; which passes it to the program it starts.
    if (pid := ProcessExist("flowd.exe"))
        DllCall("AllowSetForegroundWindow", "UInt", pid)
    try Run('"' flowExe '" ' cmd, , 'Hide', &cpid)
    catch as e {
        AhkLog("launch: could not run flow: " e.Message)
        return false
    }
    ; PROCESS_QUERY_LIMITED_INFORMATION | SYNCHRONIZE: enough to read the exit code.
    h := DllCall("OpenProcess", "UInt", 0x1000 | 0x100000, "Int", 0, "UInt", cpid, "Ptr")
    if h
        SetTimer(DispatchDone.Bind(h, 0, SpawnDone.Bind(exe, args)), -50)
    return true
}
SpawnDone(exe, args, code) {
    if (code != 0) {
        AhkLog("launch: flow spawn exited " code ", starting it here")
        RunHere(exe, args)
    }
}
; One argument quoted for the MSVC CRT / Rust argv parser: wrapped in quotes when it has
; a space, tab or quote (or is empty); a quote escaped, the backslashes before it doubled.
CrtQuote(a) {
    if (a != "" && !RegExMatch(a, '[\s"]'))
        return a
    out := '"', bs := 0
    Loop Parse a {
        if (A_LoopField = "\") {
            bs++
            continue
        }
        out .= Backslashes(A_LoopField = '"' ? bs * 2 + 1 : bs) A_LoopField
        bs := 0
    }
    return out Backslashes(bs * 2) '"'
}
Backslashes(n) {
    s := ""
    Loop n
        s .= "\"
    return s
}

; --- Bindings: the table comes from the daemon -----------------------------
; `flow query binds --plain` lists one record per line, tab-separated:
;   bind <hotkey> <scope> <kind> <admin> <chord> <help> <argv items joined by Chr(31)>
;   row <chords> <help>            an overlay row for the table
;   hardwired <chords> <help>      an overlay row for the chords below
;   option <name> 0|1              a keymap option ([keymap] in flow.toml): selection_buffer
;   source defaults|flow.toml
; The output goes through a file (UTF-8, no console window), never through
; WScript's Exec, which decodes with the OEM code page.
QueryBinds() {
    global flowExe
    tmp := A_Temp "\flow-binds-" DllCall("GetCurrentProcessId", "UInt") ".txt"
    try {
        code := RunWait(A_ComSpec ' /c ""' flowExe '" query binds --plain > "' tmp '" 2>&1"', , "Hide")
        out := FileRead(tmp, "UTF-8")
    } catch as e {
        AhkLog("binds: query failed: " e.Message)
        return ""
    }
    try FileDelete(tmp)
    if (code != 0 || !InStr(out, "source`t")) {
        AhkLog("binds: query failed (exit " code "): " SubStr(Trim(out, " `t`r`n"), 1, 200))
        return ""
    }
    return out
}

; The HotIf criteria. Named functions, so a hotkey registered under one can
; be found again (turned off, replaced) by the same function object.
TilingGate(*) {
    global DaemonUp, TilingOn
    return DaemonUp && TilingOn
}
AlwaysGate(*) {
    global DaemonUp
    return DaemonUp
}
; The gate of a registration scope: "tiling", "always", or either with "rdp-"
; for a Remote Desktop twin.
BindGate(scope) {
    switch scope {
        case "tiling": return TilingGate
        case "rdp-tiling": return RdpTilingGate
        case "rdp-always": return RdpAlwaysGate
    }
    return AlwaysGate
}

; A dispatch bind. Asking a windowed Remote Desktop for its full screen
; waits (at most 2 s) until the Windows keys are released: Remote Desktop arms
; its own keyboard hook as it switches, and a Win key still held then has its
; release eaten (a stuck Win key) or let through unmasked (the Start menu).
; docs/adr/0020-app-fullscreen.md (amendment of stage 27).
DispatchBind(cmd) {
    if (InStr(cmd, "fullscreen") && WinActive("ahk_class TscShellContainerClass")) {
        style := 0
        try style := WinGetStyle("A")
        if (style & 0xC40000) {
            KeyWait("LWin", "T2")
            KeyWait("RWin", "T2")
        }
    }
    D(cmd)
}

; What a bind runs. 0 for a kind this script does not know (a newer daemon).
BindCallback(kind, argvText, admin) {
    argv := argvText = "" ? [] : StrSplit(argvText, Chr(31))
    switch kind {
        case "dispatch":
            cmd := ""
            for a in argv
                cmd .= (cmd = "" ? "" : " ") Quote(a)
            return (*) => DispatchBind(cmd)
        case "spawn":
            return (*) => Launch(argv, admin)
        case "launcher":
            return (*) => LauncherToggle()
        case "help":
            return (*) => HelpToggle()
        case "quit":
            return (*) => StopFlow()
        case "toggle-tiling":
            return (*) => D("tiling toggle", TilingToggled)
    }
    return 0
}

; Register the listing: the previous set is turned off first (under the
; criterion each chord was registered with), then every `bind` record is
; registered under its scope's gate and the overlay rows are collected.
ApplyBinds(text) {
    global Registered, HelpRows, BindsText
    for r in Registered {
        HotIf(BindGate(r[2]))
        try Hotkey(r[1], "Off")
    }
    Registered := [], HelpRows := []
    n := 0, failed := 0
    selection := false      ; an older daemon lists no option: off
    Loop Parse text, "`n", "`r" {
        f := StrSplit(A_LoopField, "`t")
        if (f.Length < 3)
            continue
        switch f[1] {
            case "bind":
                if (f.Length < 8)
                    continue
                cb := BindCallback(f[4], f[8], f[5] = "1")
                if !cb {
                    AhkLog("binds: " f[6] ": unknown kind " f[4])
                    failed++
                    continue
                }
                HotIf(BindGate(f[3]))
                try {
                    Hotkey(f[2], cb, "On")
                    Registered.Push([f[2], f[3]])
                    n++
                } catch as e {
                    AhkLog("binds: " f[6] " (" f[2] ") not registered: " e.Message)
                    failed++
                    continue
                }
                ; The Remote Desktop twin of a Win chord (see CheckRdpFullscreen).
                if RegExMatch(f[2], "^([#^!+<>*~$]*)(.+)$", &m) && InStr(m[1], "#") {
                    twin := StrReplace(m[1], "#") m[2]
                    HotIf(BindGate("rdp-" f[3]))
                    try {
                        Hotkey(twin, RdpTwin.Bind(cb), "On")
                        Registered.Push([twin, "rdp-" f[3]])
                    } catch as e
                        AhkLog("binds: " f[6] " (" twin ", Remote Desktop twin) not registered: " e.Message)
                }
            case "row", "hardwired":
                HelpRows.Push([f[2], f[3]])
            case "option":
                if (f[2] = "selection_buffer")
                    selection := f[3] = "1"
        }
    }
    HotIf()
    SelectionApply(selection)
    BindsText := text
    BuildHelpGui()
    AhkLog("binds: " n " registered" (failed ? ", " failed " failed" : ""))
    return n
}

; flow.toml saved: the daemon reloads it by itself (once the file has been
; stable for a second); this script re-reads the listing ~2.5 s after the
; change and re-registers only if it differs. A refused reload leaves the
; daemon's listing as it was, so nothing changes here either.
CheckConfigFile() {
    global CfgPath, CfgSeen, CfgChangedAt, BindsText
    t := ""
    try t := FileGetTime(CfgPath, "M")
    if (t != CfgSeen) {
        CfgSeen := t, CfgChangedAt := A_TickCount
        AhkLog("config: flow.toml changed; re-reading the bindings shortly")
        return
    }
    if (CfgChangedAt && A_TickCount - CfgChangedAt >= 2500) {
        CfgChangedAt := 0
        text := QueryBinds()
        if (text != "" && text != BindsText) {
            n := ApplyBinds(text)
            Osd("FlowWM key bindings reloaded (" n " chords)")
        }
    }
}

AhkLog("flow: " flowExe (EnvGet("FLOW_BIN") != "" ? " (FLOW_BIN=" EnvGet("FLOW_BIN") ")" : " (FLOW_BIN unset)") " cwd=" A_WorkingDir)

; Startup gate: refuse to hook the Win key unless the daemon answers within 10 s.
Loop 20 {
    if DaemonAlive()
        break
    Sleep 500
}
if !DaemonAlive() {
    MsgBox("flow daemon not reachable at " PipeName "; not binding Win keys.", "flow.ahk", "Iconx T10")
    ExitApp()
}
TilingOn := QueryTiling()
AhkLog("startup: tiling " (TilingOn ? "on" : "off"))
DaemonUp := true
try CfgSeen := FileGetTime(CfgPath, "M")
SetTimer(CheckDaemon, 2000)

; Remote Desktop in its own full screen (Mod+F under fullscreen = "app") runs
; a low-level keyboard hook that sends the Windows key to the remote session
; (keyboardhook:i:2, the mstsc default), so Windows never sees it go down and
; AutoHotkey's own modifier state, which follows Windows', reads it as up: a
; bound Win chord would not fire. While such a full screen is active:
; - this script keeps its hook in front of mstsc's: re-installed when the full
;   screen becomes active and every RdpHookEveryMs after (in a typing pause:
;   a re-install can drop a keystroke), but never while a modifier is down or
;   right after a hotkey (a re-install forgets that a Win chord fired, and the
;   Win release then opens the Start menu);
; - it follows the Win key itself (RdpWinWatch) and every bound Win chord has a
;   twin without the Win modifier that fires only while that key is held
;   (ApplyBinds, RdpTwin). Chords the table does not bind still reach the
;   session. docs/adr/0020-app-fullscreen.md (stage 27 amendment).
RdpHookEveryMs := 1000
RdpFullscreenSeen := 0  ; the full-screen frame the hook was last re-installed for (0: none active)
RdpHookAt := 0          ; A_TickCount of that re-install
RdpDeferred := false    ; a re-install is waiting for the modifiers (logged once)
RdpHookCount := 0       ; re-installs while the current full screen is active (logged when it is left)
RdpWinDown := false     ; LWin or RWin physically down, as this script's hook saw it
RdpWinAt := 0           ; A_TickCount of its last key-down event
CheckRdpFullscreen() {
    global RdpFullscreenSeen, RdpHookAt, RdpDeferred, RdpHookEveryMs, RdpHookCount
    hwnd := RdpFullscreenActive()
    if !hwnd {
        if RdpFullscreenSeen
            AhkLog("rdp: full screen " Format("0x{:x}", RdpFullscreenSeen) " no longer active; " RdpHookCount " re-install(s) while it was")
        RdpFullscreenSeen := 0, RdpDeferred := false, RdpHookCount := 0
        return
    }
    if (hwnd = RdpFullscreenSeen && (A_TickCount - RdpHookAt < RdpHookEveryMs || A_TimeIdleKeyboard < 300))
        return
    held := RdpWinHeld() ? " Win" : ""
    for k in ["LWin", "RWin", "LControl", "RControl", "LAlt", "RAlt", "LShift", "RShift"]
        if GetKeyState(k, "P")
            held .= " " k
    since := A_TimeSincePriorHotkey
    if (held != "" || (since != "" && since >= 0 && since < 400)) {
        if !RdpDeferred
            AhkLog("rdp: full screen " Format("0x{:x}", hwnd) " re-install deferred (" (held != "" ? "held:" held : "hotkey " since " ms ago") ")")
        RdpDeferred := true
        return
    }
    InstallKeybdHook(true, true)
    RdpHookCount := hwnd = RdpFullscreenSeen ? RdpHookCount + 1 : 1
    if (hwnd != RdpFullscreenSeen || RdpDeferred)
        AhkLog("rdp: full screen " Format("0x{:x}", hwnd) " active; keyboard hook re-installed in front (every " RdpHookEveryMs " ms while active)")
    RdpFullscreenSeen := hwnd, RdpHookAt := A_TickCount, RdpDeferred := false
}
; The active window when it is a Remote Desktop frame without caption or thick
; frame (its own full screen), else 0.
RdpFullscreenActive() {
    try {
        h := WinExist("A")
        if (h && WinGetClass(h) = "TscShellContainerClass" && !(WinGetStyle(h) & 0xC40000))
            return h
    }
    return 0
}
RdpWinWatch := InputHook("V L0 I")   ; listens only: V passes every key on, L0 collects nothing
RdpWinWatch.KeyOpt("{LWin}{RWin}", "N")
RdpWinWatch.OnKeyDown := RdpWinChanged.Bind(true)
RdpWinWatch.OnKeyUp := RdpWinChanged.Bind(false)
RdpWinWatch.Start()
RdpWinChanged(down, *) {
    global RdpWinDown := down, RdpWinAt
    if down
        RdpWinAt := A_TickCount
}
; The Win key is held: seen down and not up, for at most 5 s since its last
; key-down, so a release mstsc swallowed cannot leave the twins armed.
RdpWinHeld() {
    global RdpWinDown, RdpWinAt
    return RdpWinDown && A_TickCount - RdpWinAt < 5000
}
; Gates of the twins: an mstsc full screen is active, the Win key is held, and
; the scope's own gate holds.
RdpTilingGate(*) {
    return RdpWinHeld() && RdpFullscreenActive() && TilingGate()
}
RdpAlwaysGate(*) {
    return RdpWinHeld() && RdpFullscreenActive() && AlwaysGate()
}
; A twin's action. The session got the Win key down (mstsc forwarded it), so
; first mask it and release it there while mstsc still forwards keys: when the
; chord leaves the full screen the physical release stays local, and the
; session would keep a Win key held (its next "l" locks it). The mask is a
; Ctrl tap, not AutoHotkey's vkE8, which has no scan code and never reaches
; the session (its Start menu opened).
RdpTwin(cb, *) {
    Send("{Blind}{LCtrl}{LWin up}{RWin up}")
    cb()
}
SetTimer(CheckRdpFullscreen, 250)

; No bare-Win-tap swallow on purpose: the `~LWin::Send "{Blind}{vkE8}"` trick
; dropped about one chord in eight in testing (see README.md).

; --- The table ---------------------------------------------------------------
text := QueryBinds()
if (text = "") {
    AhkLog("binds: using the baked defaults")
    text := StrReplace(DefaultBindsText, "<US>", Chr(31))
}
ApplyBinds(text)

; --- Mod + left-button drag: move a window grabbed anywhere ----------------
; The script moves the window itself while the button is held (Windows' own
; move loop cannot be started from a hook hotkey, see README.md) and brackets
; the gesture with `flow dispatch drag begin` / `drag end`, so the daemon
; previews and commits a tile's drop zone and records a float's final rect.
; A click that moves less than DragDeadZonePx is an activation, not a drag
; (niri's grab threshold): no session is opened and nothing is told to the
; daemon.
#HotIf DaemonUp && TilingOn
#LButton::ModDrag()
#HotIf
DragDeadZonePx := 8     ; the cursor must travel this far before a Mod+click becomes a drag
ModDrag() {
    global flowExe, DragDeadZonePx
    ; Screen coordinates throughout: WindowAt() and WinMove compare against
    ; WinGetPos (screen) rects, and AutoHotkey v2 defaults mouse coords to the
    ; active window's client area, which would resolve the grab point inside
    ; the wrong window.
    CoordMode("Mouse", "Screen")
    ; A key between Win-down and Win-up: Windows must not open Start when the
    ; Win key is released, whenever that happens (before or after the button).
    Send "{Blind}{vkE8}"
    MouseGetPos(&mx0, &my0)
    hwnd := WindowAt(mx0, my0)
    if !hwnd
        return
    AhkLog("mod-drag " hwnd)
    try WinActivate("ahk_id " hwnd)
    WinWaitActive("ahk_id " hwnd, , 0.3)
    ; Dead zone: wait for the cursor to leave an 8 px circle around the
    ; press point while the button is still down; a release inside it is a
    ; plain Mod+click.
    while GetKeyState("LButton", "P") {
        MouseGetPos(&mx, &my)
        if ((mx - mx0) ** 2 + (my - my0) ** 2 >= DragDeadZonePx ** 2)
            break
        Sleep 10
    }
    if !GetKeyState("LButton", "P") {
        AhkLog("mod-drag " hwnd " click: activated only")
        return
    }
    code := RunWait('"' flowExe '" dispatch drag begin', , "Hide")
    if (code != 0) {
        AhkLog("mod-drag " hwnd " refused: drag begin exit " code)
        CheckDaemon()
        return
    }
    try {
        WinGetPos(&wx, &wy, , , "ahk_id " hwnd)
        while GetKeyState("LButton", "P") {          ; until the button is released, Win key or not
            MouseGetPos(&mx, &my)
            WinMove(wx + mx - mx0, wy + my - my0, , , "ahk_id " hwnd)
            Sleep 10
        }
    } catch as e
        AhkLog("mod-drag " hwnd " stopped: " e.Message)  ; the window went away mid-drag
    code := RunWait('"' flowExe '" dispatch drag end', , "Hide")
    AhkLog("mod-drag " hwnd " end exit " code)
}

; The top-level window under the point, skipping the daemon's border overlays
; (they sit above every managed window), click-through and cloaked windows,
; the desktop and the taskbar. A maximized window is not draggable: 0.
WindowAt(x, y) {
    static skip := Map("FlowBorderOverlay", 1, "Progman", 1, "WorkerW", 1, "Shell_TrayWnd", 1, "Shell_SecondaryTrayWnd", 1)
    for w in WinGetList() {
        try {
            if skip.Has(WinGetClass("ahk_id " w))
                continue
            if (WinGetExStyle("ahk_id " w) & 0x20)      ; WS_EX_TRANSPARENT: click-through (the OSD)
                continue
            cloaked := 0
            DllCall("dwmapi\DwmGetWindowAttribute", "Ptr", w, "UInt", 14, "UInt*", &cloaked, "UInt", 4)
            if cloaked                                   ; another virtual desktop, a suspended UWP app
                continue
            WinGetPos(&wx, &wy, &ww, &wh, "ahk_id " w)
            if (x < wx || x >= wx + ww || y < wy || y >= wy + wh)
                continue
            return WinGetMinMax("ahk_id " w) = 1 ? 0 : w  ; the first hit decides
        }
    }
    return 0
}

; --- Always bound while the daemon runs (tiling on or off) -----------------
#HotIf DaemonUp

; --- Pause toggle -----------------------------------------------------------
#^+t::D("tiling toggle", TilingToggled)

; --- Daemon ----------------------------------------------------------------
; Full stop. flow-resident.ahk binds Mod+Ctrl+T to start while no daemon runs.
#^t::StopFlow()
#HotIf
StopFlow() {
    Flow("stop")
    ExitApp()
}

; --- Launcher and help overlay (their chords are in the table) --------------
#Include %A_ScriptDir%\launcher.ahk
#Include %A_ScriptDir%\help.ahk

; --- Baked defaults ---------------------------------------------------------
; For a daemon that does not answer `query binds` (an older build): the
; built-in table as the daemon lists it, with the argv separator spelled
; <US>. Generated from binds.toml; a test in src/config/binds.rs keeps it in
; step (it prints the block to paste when it is out of date).
; BEGIN DEFAULT BINDS (generated)
DefaultBindsText := "
(
bind	#Left	tiling	dispatch	0	Mod+Left	focus the next window on this screen	focus<US>left
bind	#Down	tiling	dispatch	0	Mod+Down	focus the next window on this screen	focus<US>down
bind	#Up	tiling	dispatch	0	Mod+Up	focus the next window on this screen	focus<US>up
bind	#Right	tiling	dispatch	0	Mod+Right	focus the next window on this screen	focus<US>right
bind	!h	tiling	dispatch	0	Alt+H	focus the next window on this screen	focus<US>left
bind	!j	tiling	dispatch	0	Alt+J	focus the next window on this screen	focus<US>down
bind	!k	tiling	dispatch	0	Alt+K	focus the next window on this screen	focus<US>up
bind	!l	tiling	dispatch	0	Alt+L	focus the next window on this screen	focus<US>right
bind	#+Left	tiling	dispatch	0	Mod+Shift+Left	move the window along the strip (up/down inside a stacked column)	move-window<US>left
bind	#+Down	tiling	dispatch	0	Mod+Shift+Down	move the window along the strip (up/down inside a stacked column)	move-window<US>down
bind	#+Up	tiling	dispatch	0	Mod+Shift+Up	move the window along the strip (up/down inside a stacked column)	move-window<US>up
bind	#+Right	tiling	dispatch	0	Mod+Shift+Right	move the window along the strip (up/down inside a stacked column)	move-window<US>right
bind	!+h	tiling	dispatch	0	Alt+Shift+H	move the window along the strip (up/down inside a stacked column)	move-window<US>left
bind	!+j	tiling	dispatch	0	Alt+Shift+J	move the window along the strip (up/down inside a stacked column)	move-window<US>down
bind	!+k	tiling	dispatch	0	Alt+Shift+K	move the window along the strip (up/down inside a stacked column)	move-window<US>up
bind	!+l	tiling	dispatch	0	Alt+Shift+L	move the window along the strip (up/down inside a stacked column)	move-window<US>right
bind	#^Left	tiling	dispatch	0	Mod+Ctrl+Left	focus the other screen	focus-monitor<US>left
bind	#^Down	tiling	dispatch	0	Mod+Ctrl+Down	focus the other screen	focus-monitor<US>down
bind	#^Up	tiling	dispatch	0	Mod+Ctrl+Up	focus the other screen	focus-monitor<US>up
bind	#^Right	tiling	dispatch	0	Mod+Ctrl+Right	focus the other screen	focus-monitor<US>right
bind	#!Left	tiling	dispatch	0	Mod+Alt+Left	move the window to the other screen	move-to-monitor<US>left
bind	#!Down	tiling	dispatch	0	Mod+Alt+Down	move the window to the other screen	move-to-monitor<US>down
bind	#!Up	tiling	dispatch	0	Mod+Alt+Up	move the window to the other screen	move-to-monitor<US>up
bind	#!Right	tiling	dispatch	0	Mod+Alt+Right	move the window to the other screen	move-to-monitor<US>right
bind	#1	tiling	dispatch	0	Mod+1	switch to workspace 1	switch-workspace<US>1
bind	#2	tiling	dispatch	0	Mod+2	switch to workspace 2	switch-workspace<US>2
bind	#3	tiling	dispatch	0	Mod+3	switch to workspace 3	switch-workspace<US>3
bind	#4	tiling	dispatch	0	Mod+4	switch to workspace 4	switch-workspace<US>4
bind	#5	tiling	dispatch	0	Mod+5	switch to workspace 5	switch-workspace<US>5
bind	#6	tiling	dispatch	0	Mod+6	switch to workspace 6	switch-workspace<US>6
bind	#7	tiling	dispatch	0	Mod+7	switch to workspace 7	switch-workspace<US>7
bind	#8	tiling	dispatch	0	Mod+8	switch to workspace 8	switch-workspace<US>8
bind	#9	tiling	dispatch	0	Mod+9	switch to workspace 9	switch-workspace<US>9
bind	#0	tiling	dispatch	0	Mod+0	switch to workspace 10	switch-workspace<US>10
bind	#^1	tiling	dispatch	0	Mod+Ctrl+1	move the window to workspace 1	move-to-workspace<US>1
bind	#^2	tiling	dispatch	0	Mod+Ctrl+2	move the window to workspace 2	move-to-workspace<US>2
bind	#^3	tiling	dispatch	0	Mod+Ctrl+3	move the window to workspace 3	move-to-workspace<US>3
bind	#^4	tiling	dispatch	0	Mod+Ctrl+4	move the window to workspace 4	move-to-workspace<US>4
bind	#^5	tiling	dispatch	0	Mod+Ctrl+5	move the window to workspace 5	move-to-workspace<US>5
bind	#^6	tiling	dispatch	0	Mod+Ctrl+6	move the window to workspace 6	move-to-workspace<US>6
bind	#^7	tiling	dispatch	0	Mod+Ctrl+7	move the window to workspace 7	move-to-workspace<US>7
bind	#^8	tiling	dispatch	0	Mod+Ctrl+8	move the window to workspace 8	move-to-workspace<US>8
bind	#^9	tiling	dispatch	0	Mod+Ctrl+9	move the window to workspace 9	move-to-workspace<US>9
bind	#^0	tiling	dispatch	0	Mod+Ctrl+0	move the window to workspace 10	move-to-workspace<US>10
bind	#q	tiling	dispatch	0	Mod+Q	close window	close-window
bind	#m	tiling	dispatch	0	Mod+M	minimize window	minimize-window
bind	#c	tiling	dispatch	0	Mod+C	center the focused column	center
bind	#v	tiling	dispatch	0	Mod+V	toggle floating	set-window<US>cycle
bind	#+v	tiling	dispatch	0	Mod+Shift+V	switch focus between the floating and the tiled windows	switch-focus-between-floating-and-tiling
bind	#r	tiling	dispatch	0	Mod+R	cycle this screen's column widths (flow.toml [columns] presets)	width<US>next
bind	#f	tiling	dispatch	0	Mod+F	fullscreen toggle	set-window<US>fullscreen
bind	#[	tiling	dispatch	0	Mod+BracketLeft	alone: merge into the column on that side; stacked: pop out to that side	consume-or-expel<US>left
bind	#]	tiling	dispatch	0	Mod+BracketRight	alone: merge into the column on that side; stacked: pop out to that side	consume-or-expel<US>right
bind	#+b	tiling	dispatch	0	Mod+Shift+B	book / strip layout on this screen (columns out of view piled or parked)	set-layout<US>toggle
bind	#Space	always	launcher	0	Mod+Space	application launcher (apps; window - x and file - x search those instead)	
bind	#d	always	launcher	0	Mod+D	application launcher (apps; window - x and file - x search those instead)	
bind	#t	always	spawn	0	Mod+T	run alacritty	alacritty
bind	#+t	always	spawn	1	Mod+Shift+T	run alacritty as admin	alacritty
bind	#!t	always	spawn	0	Mod+Alt+T	run wt	wt
bind	#!+t	always	spawn	1	Mod+Alt+Shift+T	run wt as admin	wt
bind	#+vkBF	always	help	0	Mod+Shift+Slash	this help (Esc hides it)	
bind	#+e	always	quit	0	Mod+Shift+E	stop FlowWM	
row	Mod+Left/Down/Up/Right	focus the next window on this screen
row	Alt+H/J/K/L	focus the next window on this screen
row	Mod+Shift+Left/Down/Up/Right	move the window along the strip (up/down inside a stacked column)
row	Alt+Shift+H/J/K/L	move the window along the strip (up/down inside a stacked column)
row	Mod+Ctrl+Left/Down/Up/Right	focus the other screen
row	Mod+Alt+Left/Down/Up/Right	move the window to the other screen
row	Mod+1..9, Mod+0	switch to that workspace
row	Mod+Ctrl+1..9, Mod+Ctrl+0	move the window to that workspace
row	Mod+Q	close window
row	Mod+M	minimize window
row	Mod+C	center the focused column
row	Mod+V	toggle floating
row	Mod+Shift+V	switch focus between the floating and the tiled windows
row	Mod+R	cycle this screen's column widths (flow.toml [columns] presets)
row	Mod+F	fullscreen toggle
row	Mod+BracketLeft, Mod+BracketRight	alone: merge into the column on that side; stacked: pop out to that side
row	Mod+Shift+B	book / strip layout on this screen (columns out of view piled or parked)
row	Mod+Space, Mod+D	application launcher (apps; window - x and file - x search those instead)
row	Mod+T	run alacritty
row	Mod+Shift+T	run alacritty as admin
row	Mod+Alt+T	run wt
row	Mod+Alt+Shift+T	run wt as admin
row	Mod+Shift+Slash	this help (Esc hides it)
row	Mod+Shift+E	stop FlowWM
hardwired	Mod + left-button drag	move a window grabbed anywhere (a tile drops into the zone under the cursor, a float moves; a click without moving only activates it)
hardwired	Mod+Ctrl+T	FlowWM off (and on again, via flow-resident.ahk)
hardwired	Mod+Ctrl+Shift+T	pause toggle (off: Windows manages windows, chords fall through)
option	selection_buffer	0
source	defaults
)"
; END DEFAULT BINDS
