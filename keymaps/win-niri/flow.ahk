#Requires AutoHotkey v2.0
#SingleInstance Force
; ════════════════════════════════════════════════════════════════════════════
; FlowWM keybindings, niri flavour, Windows key as Mod, arrow keys.
;
; Mirrors niri's default binds (https://github.com/YaLTeR/niri/wiki/Configuration:-Key-Bindings)
; onto `flow dispatch` commands. Install at %USERPROFILE%\.config\flow\flow.ahk
; (together with flow-exe.ahk, launcher.ahk and help.ahk, which it #Includes)
; and start with `flow start --ahk`. Design notes, the daemon-liveness gate,
; the tiling toggle, which Windows keys are shadowed and the AutoHotkey
; lessons are in README.md next to this file.
;
;   Mod+Left/Down/Up/Right        focus the next window on this screen
;   Mod+Shift+Arrows              move the window along the strip (up/down inside a stacked column)
;   Alt+H/J/K/L, Alt+Shift+H/J/K/L  the same two, vim-style (upstream's default chords)
;   Mod+Ctrl+Arrows               focus the other screen
;   Mod+Alt+Arrows                move the window to the other screen (it takes that screen's width)
;   Mod+1..9, Mod+0               switch workspace 1..10
;   Mod+Ctrl+1..9, Mod+Ctrl+0     move window to workspace 1..10
;   Mod+Q                         close window
;   Mod+C                         center the focused column
;   Mod+V                         toggle floating (shadows Windows clipboard history)
;   Mod+R                         cycle this screen's column width: 1/3 -> 1/2 -> 1/1 (flow.toml [columns])
;   Mod+F                         fullscreen toggle
;   Mod+Minus / Mod+Equal         shrink / expand this column one step
;   Mod+[ / Mod+]                 alone in its column: merge into the column on that side;
;                                 in a stacked column: pop out into its own column on that side
;   Mod+Space, Mod+D              application launcher (launcher.ahk: apps, open windows, settings)
;   Mod+T / Mod+Shift+T           Alacritty / Alacritty as admin (UAC)
;   Mod+Alt+T / Mod+Alt+Shift+T   Windows Terminal / Windows Terminal as admin (UAC)
;   Mod+Shift+/                   key-binding overlay (help.ahk); Esc or the same chord hides it
;   Mod+Ctrl+T                    stop FlowWM fully (daemon and this script); flow-resident.ahk,
;                                 if it runs, starts FlowWM again with the same chord
;   Mod+Ctrl+Shift+T              pause toggle: `flow dispatch tiling toggle` (daemon keeps running)
;   Mod+Shift+E                   stop FlowWM (same as Mod+Ctrl+T; niri's quit chord)
;
; All chords are bound only while the daemon's pipe answers (and the
; window-management ones only while tiling is on); Win+L stays Windows.
; ════════════════════════════════════════════════════════════════════════════

#Include %A_ScriptDir%\flow-exe.ahk

flowExe := ResolveFlowExe()   ; absolute path to flow.exe (see flow-exe.ahk); quote-safe below
PipeName := EnvGet("FLOW_PIPE_NAME") || "\\.\pipe\flow"
DaemonUp := false
TilingOn := true        ; refined from `flow ping` after the startup gate
DownTicks := 0
BusyPolls := 0          ; consecutive polls that found the pipe busy (121)
MaxBusyPolls := 5       ; after this many, "busy" means wedged, not alive

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
        try FileAppend(FormatTime(, "HH:mm:ss") " " msg "`n", path)
}

; Short on-screen message in the bottom-right corner of the primary screen,
; gone after `ms`. Never TrayTip: on Windows 11 the toast holds the
; foreground lock until dismissed (see README.md).
Osd(msg, ms := 2500) {
    static gui := 0
    AhkLog("osd: " msg)
    if gui
        try gui.Destroy()
    gui := Gui("+AlwaysOnTop -Caption +ToolWindow +Owner +E0x20", "FlowWM")   ; E0x20: click-through
    gui.BackColor := 0x1E1E1E
    gui.MarginX := 16, gui.MarginY := 10
    gui.SetFont("s11 cE8E8E8", "Segoe UI")
    gui.AddText("w520", msg)
    MonitorGetWorkArea(MonitorGetPrimary(), &l, &t, &r, &b)
    gui.Show("NoActivate Hide")
    gui.GetPos(, , &w, &h)
    gui.Show("NoActivate x" (r - w - 24) " y" (b - h - 24))
    SetTimer(OsdHide.Bind(gui), -ms)
}
OsdHide(gui) {
    try gui.Hide()
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

; Terminals. Admin uses the shell's RunAs verb (UAC prompts as usual), so both
; are run by fixed absolute paths first and a PATH lookup only as the last resort.
Alacritty(admin := false) {
    static exe := ""
    if (exe = "") {
        for p in [A_ProgramFiles "\Alacritty\alacritty.exe", EnvGet("LOCALAPPDATA") "\Programs\Alacritty\alacritty.exe"]
            if FileExist(p) {
                exe := p
                break
            }
        if (exe = "") {
            try {
                out := ComObject("WScript.Shell").Exec(A_ComSpec " /c where.exe alacritty.exe").StdOut.ReadAll()
                if (Trim(out, " `t`r`n") != "")
                    exe := Trim(StrSplit(out, "`n")[1], " `t`r")
            }
        }
    }
    if (exe = "") {
        AhkLog("alacritty not found")
        Osd("Alacritty not found (Program Files, %LOCALAPPDATA%\Programs\Alacritty, PATH).")
        return
    }
    Launch(exe, admin)
}
WindowsTerminal(admin := false) {
    ; The Store app's execution alias is a fixed per-user path; only if it is
    ; absent is `wt.exe` left to the shell's own lookup.
    alias := EnvGet("LOCALAPPDATA") "\Microsoft\WindowsApps\wt.exe"
    Launch(FileExist(alias) ? alias : "wt.exe", admin)
}
Launch(exe, admin) {
    AhkLog("launch " (admin ? "(admin) " : "") exe)
    ; Start in the home folder, not wherever the daemon (and so this script) was started from.
    try Run((admin ? "*RunAs " : "") '"' exe '"', EnvGet("USERPROFILE"))
    catch as e
        if !InStr(e.Message, "cancel")       ; a declined UAC prompt is not worth a popup
            Osd("could not start " exe ": " e.Message)
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
SetTimer(CheckDaemon, 2000)
HotIf((*) => DaemonUp && TilingOn)   ; criterion for the Hotkey() registrations below
#HotIf DaemonUp && TilingOn          ; criterion for the window-management `::` hotkeys

; No bare-Win-tap swallow on purpose: the `~LWin::Send "{Blind}{vkE8}"` trick
; dropped about one chord in eight in testing (see README.md).

; --- Focus / move on this screen (Mod, Mod+Shift) --------------------------
#Left::D("focus left")
#Down::D("focus down")
#Up::D("focus up")
#Right::D("focus right")
#+Left::D("move-window left")
#+Down::D("move-window down")
#+Up::D("move-window up")
#+Right::D("move-window right")
; Vim-style duplicates of the two rows above, as in upstream's bundled keymap.
; Alt+<letter> can collide with an app's menu accelerator (Alt+H = Help in some apps) while tiling is on.
!h::D("focus left")
!j::D("focus down")
!k::D("focus up")
!l::D("focus right")
!+h::D("move-window left")
!+j::D("move-window down")
!+k::D("move-window up")
!+l::D("move-window right")

; --- Screens (Mod+Ctrl focus, Mod+Alt move) ---------------------------------
#^Left::D("focus-monitor left")
#^Right::D("focus-monitor right")
#^Down::D("focus-monitor down")
#^Up::D("focus-monitor up")
#!Left::D("move-to-monitor left")
#!Right::D("move-to-monitor right")
#!Down::D("move-to-monitor down")
#!Up::D("move-to-monitor up")

; --- Workspaces (Mod+n, Mod+Ctrl+n) ----------------------------------------
SwitchWs(n, *) => D("switch-workspace " n)
MoveWs(n, *) => D("move-to-workspace " n)
Loop 9 {
    Hotkey("#" A_Index, SwitchWs.Bind(A_Index))
    Hotkey("#^" A_Index, MoveWs.Bind(A_Index))
}
#0::D("switch-workspace 10")
#^0::D("move-to-workspace 10")

; --- Columns / windows -----------------------------------------------------
#q::D("close-window")
#c::D("center")
#v::D("set-window cycle")
#-::D("shrink-column")
#=::D("expand-column")
#r::D("width cycle")                  ; 1/3 -> 1/2 -> 1/1 for every column on this screen
#f::D("set-window fullscreen")        ; toggle
#[::D("consume-or-expel left")        ; alone: merge into the column to the left; in a stack: pop out to the left
#]::D("consume-or-expel right")

; --- Terminals -------------------------------------------------------------
#t::Alacritty()
#+t::Alacritty(true)
#!t::WindowsTerminal()
#!+t::WindowsTerminal(true)

; --- Always bound while the daemon runs (tiling on or off) -----------------
#HotIf DaemonUp

; --- Pause toggle -----------------------------------------------------------
#^+t::D("tiling toggle", TilingToggled)

; --- Daemon ----------------------------------------------------------------
; Full stop. flow-resident.ahk binds Mod+Ctrl+T to start while no daemon runs.
#^t::StopFlow()
#+e::StopFlow()
StopFlow() {
    Flow("stop")
    ExitApp()
}
#HotIf

; --- Launcher (Mod+Space, Mod+D) and help overlay (Mod+Shift+/) --------------
#Include %A_ScriptDir%\launcher.ahk
#Include %A_ScriptDir%\help.ahk
