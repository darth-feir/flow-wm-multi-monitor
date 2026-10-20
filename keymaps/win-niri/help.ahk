; ════════════════════════════════════════════════════════════════════════════
; FlowWM key-binding overlay (Mod+Shift+/). Included from flow.ahk.
;
; Toggles an always-on-top dark panel listing every chord bound in flow.ahk.
; Esc or the same chord hides it. Uses ShowCentered() from flow.ahk.
;
; The Bindings array below is the source of truth for the overlay: keep it in
; step with the hotkeys in flow.ahk / launcher.ahk.
; ════════════════════════════════════════════════════════════════════════════

Bindings := [
    ["Mod+Left/Down/Up/Right",      "focus window (hops monitors at a strip edge)"],
    ["Mod+Ctrl+Arrows",             "move window (hops monitors at a strip edge)"],
    ["Mod+Shift+Arrows",            "focus monitor"],
    ["Mod+Shift+Ctrl+Arrows",       "move window to monitor"],
    ["Mod+1..9, Mod+0",             "switch to workspace 1..10"],
    ["Mod+Ctrl+1..9, Mod+Ctrl+0",   "move window to workspace 1..10"],
    ["Mod+Q",                       "close window"],
    ["Mod+C",                       "center the focused column"],
    ["Mod+V",                       "toggle floating"],
    ["Mod+Minus / Mod+Equal",       "shrink / expand column"],
    ["Mod+Comma",                   "consume: merge window into the column to the left"],
    ["Mod+Period",                  "expel: promote window out into its own column"],
    ["Mod+Space, Mod+D",            "application launcher (apps, open windows, settings)"],
    ["Mod+T",                       "terminal"],
    ["Mod+Shift+/",                 "this help (Esc or the same chord hides it)"],
    ["Mod+Shift+T",                 "toggle tiling (off: Windows manages windows, chords fall through)"],
    ["Mod+Shift+E",                 "stop FlowWM and this script"]
]

; Two columns: chord padded to the widest chord, then the description.
HelpText() {
    w := 0
    for b in Bindings
        w := Max(w, StrLen(b[1]))
    out := ""
    for b in Bindings
        out .= Format("{:-" w "}", b[1]) "   " b[2] "`n"
    return RTrim(out, "`n")
}

HelpGui := Gui("+AlwaysOnTop -Caption +ToolWindow +Owner", "FlowWM Help")
HelpGui.BackColor := 0x1E1E1E
HelpGui.MarginX := 24, HelpGui.MarginY := 20
HelpGui.SetFont("s13 bold cFFFFFF", "Segoe UI")
HelpGui.AddText(, "FlowWM key bindings   (Mod = Win)")
HelpGui.SetFont("s12 norm cE0E0E0", "Consolas")
HelpGui.AddText("xm y+12", HelpText())
HelpGui.SetFont("s9 norm c9A9A9A", "Segoe UI")
HelpGui.AddText("xm y+14", "Esc or Mod+Shift+/ closes this overlay. Win+L is the Windows lock. Tiling off releases every chord above except Mod+Space, Mod+D, Mod+Shift+/, Mod+Shift+T and Mod+Shift+E to Windows.")
HelpGui.OnEvent("Escape", HelpHide)

HelpToggle(*) {
    AhkLog("help toggle (visible=" DllCall("IsWindowVisible", "Ptr", HelpGui.Hwnd) ")")
    if DllCall("IsWindowVisible", "Ptr", HelpGui.Hwnd) {
        HelpHide()
        return
    }
    ShowCentered(HelpGui)
}

HelpHide(*) {
    HelpGui.Hide()
}

; --- Hotkeys ---------------------------------------------------------------
#HotIf DaemonUp
; `#+/` never fires in AutoHotkey v2 (Shift changes how "/" resolves), so bind the
; physical key by virtual-key code: vkBF = VK_OEM_2 = the "/ ?" key on US layouts.
#+vkBF::HelpToggle()
#HotIf

#HotIf IsSet(HelpGui) && WinActive("ahk_id " HelpGui.Hwnd)
Escape::HelpHide()
#HotIf
