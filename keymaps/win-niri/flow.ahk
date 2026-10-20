#Requires AutoHotkey v2.0
#SingleInstance Force
; ════════════════════════════════════════════════════════════════════════════
; FlowWM keybindings, niri flavour, Windows key as Mod, arrow keys.
;
; Mirrors niri's default binds (https://github.com/YaLTeR/niri/wiki/Configuration:-Key-Bindings)
; onto `flow dispatch` commands. Install at %USERPROFILE%\.config\flow\flow.ahk
; (together with launcher.ahk and help.ahk, which it #Includes) and start with
; `flow start --ahk`.
;
;   Mod+Left/Down/Up/Right        focus                (hops monitors at a strip edge)
;   Mod+Ctrl+Arrows               move window          (hops monitors at a strip edge)
;   Mod+Shift+Arrows              focus monitor
;   Mod+Shift+Ctrl+Arrows         move window to monitor
;   Mod+1..9, Mod+0               switch workspace 1..10
;   Mod+Ctrl+1..9, Mod+Ctrl+0     move window to workspace 1..10
;   Mod+Q                         close window
;   Mod+C                         center the focused column
;   Mod+V                         toggle floating (shadows Windows clipboard history)
;   Mod+Minus / Mod+Equal         shrink / expand column
;   Mod+Comma                     consume: merge the window into the column to the left
;   Mod+Period                    expel: promote the window out into its own column
;   Mod+Space, Mod+D              application launcher (launcher.ahk: apps, open windows, settings)
;   Mod+T / Mod+Shift+T           Alacritty / Alacritty as admin (UAC)
;   Mod+Alt+T / Mod+Alt+Shift+T   Windows Terminal / Windows Terminal as admin (UAC)
;   Mod+Shift+/                   key-binding overlay (help.ahk); Esc or the same chord hides it
;   Mod+Ctrl+T                    tiling toggle: `flow dispatch tiling toggle`
;   Mod+Shift+E                   stop FlowWM (and this script)
;
; Tiling toggle: while tiling is off every window-management chord above
; (focus, move, monitors, workspaces, close, center, float, width, consume/
; expel) is released to Windows, so Win+Arrows snap, Win+1..9 launch from the
; taskbar, and so on. Only the toggle itself, the launcher, the help overlay
; and Mod+Shift+E stay bound. The state is read from `flow ping` at startup
; ("tiling off" in its output) and flipped locally whenever the toggle
; dispatch exits 0; a TrayTip reports each flip.
;
; Fail-back: every binding above is only active while the flowd daemon's named
; pipe exists. If the daemon dies, the Win key and all chords revert to stock
; Windows within one poll (2 s) — or at once, because a chord whose dispatch
; fails re-checks the daemon on the spot — and after ~10 s the script exits
; itself. A pipe that stays busy for 5 consecutive polls (a wedged daemon)
; counts as down too. At startup the script refuses to hook anything unless
; the daemon answers.
;
; Win+L (lock) is handled by Windows below the hook layer and is untouched;
; the only ways around that are a keyboard-firmware or registry remap of the
; left Win key (the FlowWM README's "sacrificial key" setup), neither of which
; is used here. Not bound here, so still Windows: Win+Tab, Win+E, Win+R,
; Win+Shift+S, Win+A/I/X/S/N/W/P, Win+Ctrl+Space. Shadowed while the daemon
; runs: Win+1..9/0 (taskbar launch), Win+C, Win+Q, Win+V, Win+D (show
; desktop), Win+T (taskbar cycle), Win+Minus/Equal (Magnifier), Win+Comma
; (peek), Win+Period (emoji), Win+Arrows (Snap), Win+Ctrl+Left/Right (virtual
; desktops), Win+Shift+Left/Right (Windows' own move-to-monitor), Win+Space
; (Windows' input-language switcher; Win+Ctrl+Space, which cycles the other
; way, still works). With tiling off only Win+Space, Win+D, Win+Shift+/,
; Win+Ctrl+T and Win+Shift+E remain shadowed.
; ════════════════════════════════════════════════════════════════════════════

flowExe := "flow"      ; path to flow.exe (PATH by default); quote-safe below
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
        TrayTip("could not run flow: " e.Message, "flow.ahk")
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

; onExit (optional): a function object called as onExit(exitCode) once the
; dispatch has finished (not called if the process could not be opened or is
; still running after ~5 s).
D(cmd, onExit := 0) {
    global flowExe
    AhkLog("dispatch " cmd)
    try Run('"' flowExe '" dispatch ' cmd, , 'Hide', &pid)
    catch as e {
        TrayTip("could not run flow: " e.Message, "flow.ahk")
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
        TrayTip("tiling toggle failed (flow exit " code ")", "flow.ahk")
        return
    }
    TilingOn := !TilingOn
    AhkLog("tiling " (TilingOn ? "on" : "off"))
    TrayTip(TilingOn ? "FlowWM tiling on" : "FlowWM tiling off: Windows is managing windows", "flow.ahk")
}

; --- Daemon liveness --------------------------------------------------------
DaemonAlive() {
    global PipeName, BusyPolls, MaxBusyPolls
    ; 1 ms wait: nonzero = an instance is free. A_LastError 2 = no such pipe
    ; (daemon gone); 121 (ERROR_SEM_TIMEOUT) = pipe exists but busy. Busy is
    ; alive only for a bounded run of polls: a wedged daemon holds the pipe
    ; busy forever, and must count as down so the Win key is released.
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
        TrayTip("FlowWM daemon is gone; Win key released.", "flow.ahk")
        Sleep 1500
        ExitApp()
    }
}

; --- Shared GUI helper (used by launcher.ahk and help.ahk) ------------------
; Show a Gui centred on the work area of the monitor that holds the active
; window (primary monitor if there is none), then make sure it is the active
; window: Windows does not always grant the foreground to a window shown from
; a chord while the Win key is still held, so retry for ~0.5 s. Measures the
; window first via a hidden Show so auto-sized controls are already laid out.
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

; Terminals. Mod+T Alacritty, Mod+Shift+T Alacritty as admin, Mod+Alt+T
; Windows Terminal, Mod+Alt+Shift+T Windows Terminal as admin. Admin uses the
; shell's RunAs verb, so UAC prompts as usual. Alacritty is looked up in its
; usual install folders and then on PATH.
Alacritty(admin := false) {
    static exe := ""
    if (exe = "") {
        for p in [A_AppData "\..\Local\Programs\Alacritty\alacritty.exe", A_ProgramFiles "\Alacritty\alacritty.exe"]
            if FileExist(p) {
                exe := p
                break
            }
        if (exe = "") {
            try {
                out := ComObject("WScript.Shell").Exec("where.exe alacritty.exe").StdOut.ReadAll()
                if (Trim(out, " `t`r`n") != "")
                    exe := Trim(StrSplit(out, "`n")[1], " `t`r")
            }
        }
    }
    if (exe = "") {
        AhkLog("alacritty not found")
        TrayTip("Alacritty not found (%LOCALAPPDATA%\Programs\Alacritty, Program Files, PATH).", "flow.ahk")
        return
    }
    Launch(exe, admin)
}
WindowsTerminal(admin := false) {
    Launch("wt.exe", admin)
}
Launch(exe, admin) {
    AhkLog("launch " (admin ? "(admin) " : "") exe)
    try Run((admin ? "*RunAs " : "") '"' exe '"')
    catch as e
        if !InStr(e.Message, "cancel")       ; a declined UAC prompt is not worth a popup
            TrayTip("could not start " exe ": " e.Message, "flow.ahk")
}

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

; No bare-Win-tap swallow on purpose: AutoHotkey already masks the Start menu
; after every Win chord, and the `~LWin::Send "{Blind}{vkE8}"` trick dropped
; roughly one chord in eight in testing (its own key send lands between the
; Win press and the arrow). A bare Win tap therefore still opens Start.

; --- Focus / move (Mod, Mod+Ctrl) ------------------------------------------
#Left::D("focus left")
#Down::D("focus down")
#Up::D("focus up")
#Right::D("focus right")
#^Left::D("move-window left")
#^Down::D("move-window down")
#^Up::D("move-window up")
#^Right::D("move-window right")

; --- Monitors (Mod+Shift, Mod+Shift+Ctrl) ----------------------------------
#+Left::D("focus-monitor left")
#+Right::D("focus-monitor right")
#+Down::D("focus-monitor down")
#+Up::D("focus-monitor up")
#+^Left::D("move-to-monitor left")
#+^Right::D("move-to-monitor right")
#+^Down::D("move-to-monitor down")
#+^Up::D("move-to-monitor up")

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
#,::D("merge-column left")
#.::D("promote right")

; --- Terminals -------------------------------------------------------------
#t::Alacritty()
#+t::Alacritty(true)
#!t::WindowsTerminal()
#!+t::WindowsTerminal(true)

; --- Always bound while the daemon runs (tiling on or off) -----------------
#HotIf DaemonUp

; --- Tiling toggle ----------------------------------------------------------
#^t::D("tiling toggle", TilingToggled)

; --- Daemon ----------------------------------------------------------------
#+e:: {
    Flow("stop")
    ExitApp()
}
#HotIf

; --- Launcher (Mod+Space, Mod+D) and help overlay (Mod+Shift+/) --------------
#Include %A_ScriptDir%\launcher.ahk
#Include %A_ScriptDir%\help.ahk
