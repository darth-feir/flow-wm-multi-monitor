; ════════════════════════════════════════════════════════════════════════════
; FlowWM key-binding overlay (the `show-hotkey-overlay` action, Win+Shift+/
; by default). Included from flow.ahk.
;
; Toggles an always-on-top dark panel listing every chord the daemon bound
; (`flow query binds`) and the hardwired ones. The rows come from the same
; listing flow.ahk registers its hotkeys from, so the overlay can never
; disagree with the chords; BuildHelpGui() rebuilds the panel whenever the
; binds are (re)loaded. Esc hides it. Uses ShowCentered() from flow.ahk.
; ════════════════════════════════════════════════════════════════════════════
; (HelpGui itself is declared in flow.ahk's variable block: ApplyBinds() builds
; the panel before this include's auto-execute lines would have run.)

; Two columns: chord padded to the widest chord, then the description.
HelpText(rows) {
    w := 0
    for r in rows
        w := Max(w, StrLen(r[1]))
    out := ""
    for r in rows
        out .= Format("{:-" w "}", r[1]) "   " r[2] "`n"
    return RTrim(out, "`n")
}

; (Re)build the panel from HelpRows (set by ApplyBinds in flow.ahk).
BuildHelpGui() {
    global HelpGui, HelpRows
    if IsObject(HelpGui)
        try HelpGui.Destroy()
    HelpGui := Gui("+AlwaysOnTop -Caption +ToolWindow +Owner", "FlowWM Help")
    HelpGui.BackColor := 0x1E1E1E
    HelpGui.MarginX := 24, HelpGui.MarginY := 20
    HelpGui.SetFont("s13 bold cFFFFFF", "Segoe UI")
    HelpGui.AddText(, "FlowWM key bindings")
    HelpGui.SetFont("s12 norm cE0E0E0", "Consolas")
    HelpGui.AddText("xm y+12", HelpRows.Length ? HelpText(HelpRows) : "(no bindings loaded)")
    HelpGui.SetFont("s9 norm c9A9A9A", "Segoe UI")
    HelpGui.AddText("xm y+14", "Esc closes this overlay. Win+L is the Windows lock. Tiling off (Win+Ctrl+Shift+T) releases the window-management chords to Windows; the launcher, this help, programs, Win+Ctrl+T and the stop chord stay bound. Bindings: [binds] in flow.toml, applied on save.")
    HelpGui.OnEvent("Escape", HelpHide)
}

HelpToggle(*) {
    global HelpGui
    if !IsObject(HelpGui)
        BuildHelpGui()
    AhkLog("help toggle (visible=" DllCall("IsWindowVisible", "Ptr", HelpGui.Hwnd) ")")
    if DllCall("IsWindowVisible", "Ptr", HelpGui.Hwnd) {
        HelpHide()
        return
    }
    ShowCentered(HelpGui)
}

HelpHide(*) {
    global HelpGui
    if IsObject(HelpGui)
        HelpGui.Hide()
}

; --- Hotkeys ---------------------------------------------------------------
; The chord that opens the overlay is in the binds table (flow.ahk registers
; it); only Esc inside the panel is bound here.
#HotIf IsObject(HelpGui) && WinActive("ahk_id " HelpGui.Hwnd)
Escape::HelpHide()
#HotIf
