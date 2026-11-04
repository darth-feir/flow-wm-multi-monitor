#Requires AutoHotkey v2.0
; ════════════════════════════════════════════════════════════════════════════
; FlowWM selection buffer: the X11 "primary selection" on Windows. Included
; from flow.ahk (so it lives and dies with the keymap); flow.ahk calls
; SelectionApply(on) whenever it (re)reads the daemon's listing, whose
; `option selection_buffer 0|1` record carries `[keymap] selection_buffer`.
; Off (the default): no hotkey of this file exists.
;
; On:
;   left-button release after a drag (> 8 px) or a double/triple click
;       remember the selected text of the focused element, else of the
;       element under the pointer, else of their ancestors (a page's Document),
;       read through UI Automation (a classic Edit control through EM_GETSEL);
;       never the clipboard; nothing from a password field; an empty answer
;       keeps the buffer
;   the same in Alacritty (it has no UI Automation text)
;       take what Alacritty's copy-on-select (`[selection] save_to_clipboard = true`) wrote to the clipboard,
;       then put back the clipboard as it was on the press (SelAlCapture)
;   middle click (no modifier) with a non-empty buffer over a text field
;   or a terminal
;       click there, put the buffer on the clipboard for one paste key
;       (Shift+Insert, Ctrl+Shift+V in SelPasteKeys), restore the clipboard
;   any other middle click
;       passed on unchanged (replayed when the buffer was non-empty)
;
; The rules, the lists and the limits are in README.md (*Selection buffer*);
; the reasons in docs/adr/0026-selection-buffer.md. The trace (keymap_log)
; records lengths and executables, never the text.
; ════════════════════════════════════════════════════════════════════════════

SelOn := false          ; [keymap] selection_buffer as last applied
SelBuffer := ""         ; the remembered text
SelUia := 0             ; the CUIAutomation object, made on first use
SelDragPx := 8          ; a press-release farther apart than this is a drag
SelMaxChars := 1048576  ; at most this much of a selection is read
SelPasteWaitMs := 300   ; how long the buffer stays on the clipboard after the paste key
SelDownX := 0, SelDownY := 0, SelDownAt := 0, SelClicks := 0, SelArmed := false
SelWalkHops := 24       ; ancestors asked per walk, from the pointer and from the focus (SelWalk)
SelWalkMs := 1500       ; a capture asks no further element after this long
SelWarm := Map()        ; browser windows whose accessibility tree a first press has woken (SelWarmUp)
; Alacritty: its copy-on-select is the signal (ADR-0026, *Alacritty*).
SelAlWaitMs := 300      ; how long to wait for Alacritty's clipboard write after the release
SelAlPollMs := 20
SelAlSnap := 0          ; ClipboardAll() from the first press of a click sequence in Alacritty
SelAlSeq := 0           ; the clipboard sequence number Alacritty's write is detected against
SelAlDown := false      ; the last press was in Alacritty
SelAlBusy := false      ; an Alacritty capture is waiting or reading
SelAlAgain := ""        ; a release that came while it ran: capture again afterwards
SelPasteGen := 0        ; counts pastes (a paste during an Alacritty capture voids its read)

; Middle click always belongs to these windows (remote sessions, the shell).
SelSkipExes := Map("mstsc.exe", 1, "msrdc.exe", 1, "vmconnect.exe", 1)
SelSkipClasses := Map("Shell_TrayWnd", 1, "Shell_SecondaryTrayWnd", 1, "Progman", 1, "WorkerW", 1, "FlowBorderOverlay", 1)
; Terminals: paste without asking UI Automation what is under the pointer.
SelTermExes := Map("alacritty.exe", 1, "wezterm-gui.exe", 1, "windowsterminal.exe", 1, "mintty.exe", 1, "putty.exe", 1)
SelTermClasses := Map("CASCADIA_HOSTING_WINDOW_CLASS", 1, "ConsoleWindowClass", 1, "mintty", 1, "PuTTY", 1, "VirtualConsoleClass", 1)
; Browsers: a page is a document too; paste only into what says it is writable.
SelBrowsers := Map("msedge.exe", 1, "chrome.exe", 1, "firefox.exe", 1, "brave.exe", 1, "vivaldi.exe", 1, "opera.exe", 1)
; The paste key where Shift+Insert does not paste the clipboard.
SelPasteKeys := Map("alacritty.exe", "^+v")

SelLog(msg) {
    static path := EnvGet("FLOW_AHK_LOG")
    if path
        try FileAppend(FormatTime(, "HH:mm:ss") "." A_MSec " " msg "`n", path)
}

; Turn the buffer on or off: (un)register its hotkeys; off forgets the text.
SelectionApply(on) {
    global SelOn, SelBuffer, SelAlSnap, SelAlBusy, SelAlAgain
    on := on ? true : false
    if (on = SelOn)
        return
    SelOn := on
    state := on ? "On" : "Off"
    HotIf()
    Hotkey("~LButton", SelDown, state)
    Hotkey("~+LButton", SelDown, state)
    Hotkey("~*LButton Up", SelUp, state)
    HotIf(SelPasteGate)
    Hotkey("MButton", SelPaste, state)
    HotIf()
    if !on
        SelBuffer := "", SelAlSnap := 0, SelAlBusy := false, SelAlAgain := ""
    SelLog("selection: " (on ? "on" : "off"))
}

; --- Capture -----------------------------------------------------------------
SelDown(*) {
    global SelDownX, SelDownY, SelDownAt, SelClicks, SelArmed, SelAlDown, SelAlSnap, SelAlSeq, SelAlBusy, SelBrowsers, SelWarm
    CoordMode("Mouse", "Screen")
    MouseGetPos(&x, &y, &win)
    now := A_TickCount
    exe := SelExe(win)
    al := exe = "alacritty.exe"
    if (win && SelBrowsers.Has(exe) && !SelWarm.Has(win)) {
        if (SelWarm.Count > 64)
            SelWarm.Clear()
        SelWarm[win] := A_TickCount
        SetTimer(SelWarmUp.Bind(win, exe), -1)
    }
    near := Abs(x - SelDownX) <= 4 && Abs(y - SelDownY) <= 4
    ; Alacritty counts a multi-click by time only, whatever the distance.
    if (SelClicks && now - SelDownAt <= DllCall("GetDoubleClickTime", "UInt") && (near || (al && SelAlDown)))
        SelClicks += 1
    else
        SelClicks := 1
    SelDownX := x, SelDownY := y, SelDownAt := now, SelArmed := true, SelAlDown := al
    ; Before Alacritty can write (a double click writes on its second press already): the first press
    ; of a sequence only, and never while a capture still holds the snapshot.
    if (al && SelClicks = 1 && !SelAlBusy) {
        SelAlSnap := ClipboardAll()
        SelAlSeq := DllCall("GetClipboardSequenceNumber", "UInt")
    } else if (!al && !SelAlBusy)
        SelAlSnap := 0
}

SelUp(*) {
    global SelDownX, SelDownY, SelClicks, SelArmed, SelDragPx, SelAlDown, SelAlBusy, SelAlSnap, SelAlAgain
    if !SelArmed
        return
    SelArmed := false
    CoordMode("Mouse", "Screen")
    MouseGetPos(&x, &y, &win)
    dragged := (x - SelDownX) ** 2 + (y - SelDownY) ** 2 > SelDragPx ** 2
    if SelAlDown {
        n := Mod(SelClicks - 1, 3) + 1                   ; Alacritty cycles single, double, triple
        if dragged
            how := "drag", SelClicks := 0
        else if (n >= 2)
            how := n = 2 ? "double" : "triple"
        else if GetKeyState("Shift")                     ; Shift+click extends its selection
            how := "shift-click"
        else
            return
        if !IsObject(SelAlSnap)
            return
        if SelAlBusy {                                   ; a triple click released while the double's capture runs
            SelAlAgain := how
            return
        }
        SelAlBusy := true
        SetTimer(SelAlCapture.Bind(how), -15)
        return
    }
    if dragged
        how := "drag", SelClicks := 0
    else if (SelClicks >= 2)
        how := SelClicks "-click"
    else
        return
    ; Off the hook thread, and late enough for the app to have updated its selection.
    SetTimer(SelCapture.Bind(x, y, win, how), -80)
}

; Alacritty draws its own text. With `[selection] save_to_clipboard = true` it writes the selection to the
; clipboard on the release (a double / triple click on the press too): wait for the sequence number to move,
; take the text, put back the snapshot SelDown took unless something wrote again after the read.
; The sequence and its limits: ADR-0026, *Alacritty*.
SelAlCapture(how) {
    global SelBuffer, SelMaxChars, SelAlWaitMs, SelAlPollMs, SelAlSnap, SelAlSeq, SelAlBusy, SelAlAgain, SelPasteGen
    gen := SelPasteGen, start := A_TickCount, moved := false
    Loop {
        if (DllCall("GetClipboardSequenceNumber", "UInt") != SelAlSeq) {
            moved := true
            break
        }
        if (A_TickCount - start >= SelAlWaitMs)
            break
        Sleep(SelAlPollMs)
    }
    if !moved {
        SelLog("selection: alacritty did not copy within " SelAlWaitMs " ms; buffer kept (" StrLen(SelBuffer) " chars)")
        SelAlNext()
        return
    }
    Loop 5 {                                             ; a double click writes twice: let the last write land
        s := DllCall("GetClipboardSequenceNumber", "UInt")
        Sleep(SelAlPollMs)
        if (DllCall("GetClipboardSequenceNumber", "UInt") = s)
            break
    }
    text := ""
    if (SelPasteGen = gen)
        try text := A_Clipboard
    read := DllCall("GetClipboardSequenceNumber", "UInt")
    if (SelPasteGen != gen)
        SelLog("selection: a paste ran during the alacritty capture; buffer kept (" StrLen(SelBuffer) " chars)")
    else if (text = "")
        SelLog("selection: alacritty wrote no text; buffer kept (" StrLen(SelBuffer) " chars)")
    else {
        SelBuffer := StrLen(text) > SelMaxChars ? SubStr(text, 1, SelMaxChars) : text
        SelLog("selection: captured " StrLen(SelBuffer) " chars (" how ", alacritty copy-on-select) from alacritty.exe")
    }
    if (DllCall("GetClipboardSequenceNumber", "UInt") = read) {
        A_Clipboard := SelAlSnap
        SelAlSeq := DllCall("GetClipboardSequenceNumber", "UInt")
        SelLog("selection: clipboard restored (" SelAlSnap.Size " bytes)")
    } else {
        SelAlSeq := read                                 ; a queued capture still sees the newer write
        SelLog("selection: the clipboard changed again after the read; not restored")
    }
    SelAlNext()
}
; End an Alacritty capture, or run the one a release queued meanwhile.
SelAlNext() {
    global SelAlBusy, SelAlAgain
    if (SelAlAgain != "") {
        how := SelAlAgain, SelAlAgain := ""
        SetTimer(SelAlCapture.Bind(how), -15)
        return
    }
    SelAlBusy := false
}

SelCapture(x, y, win, how, again := false) {
    global SelBuffer, SelWarm
    if (!win || SelOwnWindow(win))
        return
    if !(uia := SelUiaGet())
        return
    exe := SelExe(win)
    ; The focused element, the element under the pointer, then the ancestors of each (a web page keeps its
    ; selection on its Document): ADR-0026, *Where the selection lives*.
    r := {text: "", pw: false, from: "", start: A_TickCount, focus: [], pointer: []}
    fe := 0, pe := 0, walker := 0
    try ComCall(8, uia, "Ptr*", &fe)                     ; IUIAutomation::GetFocusedElement
    fct := 0, pct := 0
    if fe
        fct := SelAsk(fe, "focus", r, true)
    if (r.text = "" && !r.pw) {
        try ComCall(7, uia, "Int64", SelPoint(x, y), "Ptr*", &pe)   ; ElementFromPoint
        if pe
            pct := SelAsk(pe, "pointer", r, true)
    }
    if (r.text = "" && !r.pw && (fe || pe))
        try ComCall(14, uia, "Ptr*", &walker)            ; IUIAutomation::get_ControlViewWalker
    ; A Document or Window asked first is the top already.
    if (r.text = "" && !r.pw && pe && walker && pct != 50030 && pct != 50032)
        SelWalk(walker, pe, "pointer", r)
    if (r.text = "" && !r.pw && fe && walker && fct != 50030 && fct != 50032)
        SelWalk(walker, fe, "focus", r)
    for p in [fe, pe, walker]
        if p
            ObjRelease(p)
    text := r.text, pw := r.pw, from := r.from
    if (text = "" && !pw) {
        from := "edit control"
        try text := SelEditControlText(&pw)
    }
    if pw {
        SelLog("selection: " how " in a password field of " exe "; not captured")
        return
    }
    ; A browser tree the press has only just woken may not hold the selection yet: once more, later.
    if (text = "" && !again && SelWarm.Has(win) && A_TickCount - SelWarm[win] < 3000) {
        SelLog("selection: " how " in " exe ": nothing yet (focus " SelChain(r.focus) "; pointer " SelChain(r.pointer) "); the accessibility tree was just woken, reading again in 400 ms")
        SetTimer(SelCapture.Bind(x, y, win, how, true), -400)
        return
    }
    if (text = "") {
        SelLog("selection: " how " in " exe ": no selected text (focus " SelChain(r.focus) "; pointer " SelChain(r.pointer) "); buffer kept (" StrLen(SelBuffer) " chars)")
        return
    }
    SelBuffer := text
    SelLog("selection: captured " StrLen(text) " chars (" how ", " from ") from " exe)
}

; Walk up from el (not released here) through the control view and ask each ancestor. Stops at a
; selection, a password element, the first Document (a page's selection is its own), a Window (the top
; of the app), after SelWalkHops ancestors, or once the capture has run SelWalkMs.
SelWalk(walker, el, chain, r) {
    global SelWalkHops, SelWalkMs
    cur := el
    Loop SelWalkHops {
        if (A_TickCount - r.start > SelWalkMs) {
            r.%chain%.Push("(out of time)")
            break
        }
        p := 0
        try ComCall(3, walker, "Ptr", cur, "Ptr*", &p)   ; IUIAutomationTreeWalker::GetParentElement
        if (cur != el)
            ObjRelease(cur)
        cur := p
        if !cur
            break
        ct := SelAsk(cur, chain, r, false)
        if (r.text != "" || r.pw || ct = 50030 || ct = 50032)   ; Document, Window
            break
    }
    if (cur && cur != el)
        ObjRelease(cur)
}

; Ask one element for its selection; a hit sets r.text and r.from, a password element r.pw. Otherwise its
; control type and why it gave nothing go to r.<chain> for the trace. Returns the control type.
SelAsk(el, chain, r, first) {
    ct := 0, pw := false, state := ""
    try ComCall(21, el, "Int*", &ct)                     ; get_CurrentControlType
    try text := SelSelectedText(el, &pw, &state)
    catch
        text := "", state := "error"
    name := SelCtName(ct)
    if pw {
        r.pw := true
        r.%chain%.Push(name " password")
    } else if (text != "") {
        r.text := text
        r.from := first ? chain : chain " ancestor " name
    } else if (state = "no-text")
        r.%chain%.Push(first ? name " no-text" : name)
    else
        r.%chain%.Push(name " " state)
    return ct
}

; One chain for the trace: "Text empty -> Group x3 -> Document empty", or "none".
SelChain(list) {
    out := "", prev := "", n := 0
    for item in list {
        if (item = prev) {
            n += 1
            continue
        }
        if (prev != "")
            out .= (out = "" ? "" : " -> ") prev (n > 1 ? " x" n : "")
        prev := item, n := 1
    }
    if (prev = "")
        return "none"
    return out (out = "" ? "" : " -> ") prev (n > 1 ? " x" n : "")
}

SelCtName(ct) {
    static names := ["Button", "Calendar", "CheckBox", "ComboBox", "Edit", "Hyperlink", "Image", "ListItem", "List", "Menu"
        , "MenuBar", "MenuItem", "ProgressBar", "RadioButton", "ScrollBar", "Slider", "Spinner", "StatusBar", "Tab", "TabItem"
        , "Text", "ToolBar", "ToolTip", "Tree", "TreeItem", "Custom", "Group", "Thumb", "DataGrid", "DataItem"
        , "Document", "SplitButton", "Window", "Pane", "Header", "HeaderItem", "Table", "TitleBar", "Separator", "SemanticZoom", "AppBar"]
    i := ct - 49999
    return (i >= 1 && i <= names.Length) ? names[i] : "type " ct
}

; The selected text of a UIA element ("" when none); pw is set for a password field. state says why it is
; empty: "no-text" (no text pattern), "empty" (no selection there), "error" (the call failed).
SelSelectedText(el, &pw, &state := "") {
    global SelMaxChars
    pw := false, state := "empty"
    isPw := 0
    try ComCall(35, el, "Int*", &isPw)                   ; IUIAutomationElement::get_CurrentIsPassword
    if isPw {
        pw := true
        return ""
    }
    if !(tp := SelPattern(el, 10014, "{32eba289-3583-42c9-9c59-3b6d9a1e9b6a}")) {  ; TextPattern
        state := "no-text"
        return ""
    }
    text := "", arr := 0
    try {
        ComCall(5, tp, "Ptr*", &arr)                     ; IUIAutomationTextPattern::GetSelection
        n := 0
        ComCall(3, arr, "Int*", &n)                      ; IUIAutomationTextRangeArray::get_Length
        Loop n {
            range := 0, bstr := 0
            ComCall(4, arr, "Int", A_Index - 1, "Ptr*", &range)
            if !range
                continue
            try ComCall(12, range, "Int", SelMaxChars, "Ptr*", &bstr)   ; IUIAutomationTextRange::GetText
            ObjRelease(range)
            if bstr {
                part := StrGet(bstr, "UTF-16")
                DllCall("oleaut32\SysFreeString", "Ptr", bstr)
                if (part != "")
                    text .= (text = "" ? "" : "`n") part
            }
        }
    } catch
        state := "error"
    if arr
        ObjRelease(arr)
    ObjRelease(tp)
    return text
}

; A browser builds its accessibility tree on the first UI Automation request, and a selection made before
; that reads as none (Firefox, ADR-0026). The first press in each browser window asks for the window's
; element and the focused element once, off the hook, so the tree is up when the selection is read.
SelWarmUp(win, exe) {
    if (!(uia := SelUiaGet()) || SelOwnWindow(win))
        return
    t := A_TickCount, el := 0, fe := 0
    try ComCall(6, uia, "Ptr", win, "Ptr*", &el)         ; IUIAutomation::ElementFromHandle
    try ComCall(8, uia, "Ptr*", &fe)                     ; GetFocusedElement
    for p in [el, fe]
        if p
            ObjRelease(p)
    SelLog("selection: woke the accessibility tree of " exe " (" (A_TickCount - t) " ms)")
}

; The selection of the focused classic Edit control (Win32 dialogs, WinForms TextBox): such a
; control may answer UI Automation through its own MSAA object, which has no text pattern.
; EM_GETSEL and WM_GETTEXT work across processes; ES_PASSWORD sets pw. RichEdit is left to UIA
; (its EM_GETSEL counts line breaks differently from its WM_GETTEXT).
SelEditControlText(&pw) {
    pw := false
    if !(fg := WinExist("A"))
        return ""
    tid := DllCall("GetWindowThreadProcessId", "Ptr", fg, "Ptr", 0, "UInt")
    gti := Buffer(A_PtrSize = 8 ? 72 : 48, 0)                  ; GUITHREADINFO
    NumPut("UInt", gti.Size, gti, 0)
    if !DllCall("GetGUIThreadInfo", "UInt", tid, "Ptr", gti)
        return ""
    if !(focus := NumGet(gti, A_PtrSize = 8 ? 16 : 12, "Ptr")) ; hwndFocus
        return ""
    if !RegExMatch(WinGetClass("ahk_id " focus), "i)^(Edit|WindowsForms10\.EDIT\..*)$")
        return ""
    if (WinGetStyle("ahk_id " focus) & 0x20) {                  ; ES_PASSWORD
        pw := true
        return ""
    }
    sel := SendMessage(0xB0, 0, 0, focus, , , , , 1000)        ; EM_GETSEL: start | end << 16
    a := sel & 0xFFFF, b := (sel >> 16) & 0xFFFF
    if (b <= a)
        return ""
    text := ControlGetText(focus)
    if (StrLen(text) > 0xFFFF)                                  ; the packed positions would be cut off
        return ""
    return SubStr(text, a + 1, b - a)
}

; --- Paste -------------------------------------------------------------------
; The hotkey's criterion: cheap checks only (it runs while the mouse hook waits).
SelPasteGate(*) {
    global SelOn, SelBuffer, SelSkipExes, SelSkipClasses
    if (!SelOn || SelBuffer = "")
        return false
    MouseGetPos(, , &win)
    if !win
        return false
    try {
        if SelSkipClasses.Has(WinGetClass("ahk_id " win))
            return false
        if SelSkipExes.Has(StrLower(WinGetProcessName("ahk_id " win)))
            return false
        if SelOwnWindow(win)
            return false
    } catch
        return false
    return true
}

SelPaste(*) {
    global SelBuffer, SelPasteKeys, SelPasteWaitMs, SelPasteGen
    CoordMode("Mouse", "Screen")
    MouseGetPos(&x, &y, &win)
    exe := SelExe(win), cls := ""
    try cls := WinGetClass("ahk_id " win)
    kind := SelIsTerminal(exe, cls) ? "terminal" : SelEditableAt(x, y, exe)
    if (SubStr(kind, 1, 3) = "no:") {
        SelLog("selection: middle click passed on (" SubStr(kind, 4) ", " exe ")")
        SelReplayMiddle()
        return
    }
    if (kind = "terminal") {
        try WinActivate("ahk_id " win)
        try WinWaitActive("ahk_id " win, , 0.3)
    } else
        Click(x, y)                                      ; focus the field and put the caret under the pointer
    keys := SelPasteKeys.Has(exe) ? SelPasteKeys[exe] : "+{Insert}"
    text := SelBuffer
    SelPasteGen += 1
    saved := ClipboardAll()
    if !SelClipboardSet(text) {
        SelLog("selection: clipboard busy; nothing pasted into " exe)
        return
    }
    Sleep(30)
    Send(keys)
    Sleep(SelPasteWaitMs)
    A_Clipboard := saved
    SelLog("selection: pasted " StrLen(text) " chars into " exe " (" kind ", " keys "); clipboard restored (" saved.Size " bytes)")
}

; Give a suppressed middle click back to the app: down now, up when the button is.
SelReplayMiddle() {
    Send("{Blind}{MButton down}")
    KeyWait("MButton")
    Send("{Blind}{MButton up}")
}

SelIsTerminal(exe, cls) {
    global SelTermExes, SelTermClasses
    return SelTermExes.Has(exe) || SelTermClasses.Has(cls)
}

; What is under the pointer: "edit" / "document" to paste, or "no:<why>".
; Walks up at most three parents from the element hit (a text run inside a field).
SelEditableAt(x, y, exe) {
    global SelBrowsers
    if !(uia := SelUiaGet())
        return "no:no UI Automation"
    el := 0
    try ComCall(7, uia, "Int64", SelPoint(x, y), "Ptr*", &el)       ; ElementFromPoint
    catch
        return "no:UI Automation refused"
    if !el
        return "no:nothing under the pointer"
    strict := SelBrowsers.Has(exe)
    walker := 0
    try ComCall(14, uia, "Ptr*", &walker)                ; IUIAutomation::get_ControlViewWalker
    result := "no:not a text field"
    Loop 4 {
        verdict := ""
        try verdict := SelEditable(el, strict)
        if (verdict != "") {
            result := verdict
            break
        }
        parent := 0
        if walker
            try ComCall(3, walker, "Ptr", el, "Ptr*", &parent)   ; IUIAutomationTreeWalker::GetParentElement
        ObjRelease(el)
        el := parent
        if !el
            break
    }
    if el
        ObjRelease(el)
    if walker
        ObjRelease(walker)
    return result
}

; One element: "" = not a text field (look at its parent), "edit", "document", or "no:<why>".
SelEditable(el, strict) {
    pw := 0, ct := 0
    ComCall(35, el, "Int*", &pw)                         ; get_CurrentIsPassword
    if pw
        return "no:password field"
    ComCall(21, el, "Int*", &ct)                         ; get_CurrentControlType
    if (ct != 50004 && ct != 50030)                      ; Edit, Document
        return ""
    ro := -1                                             ; -1: no ValuePattern
    if (vp := SelPattern(el, 10002, "{a94cd8b1-0844-4cd6-9d2d-640537ab39e9}")) {
        v := 0
        try {
            ComCall(5, vp, "Int*", &v)                   ; IUIAutomationValuePattern::get_CurrentIsReadOnly
            ro := v ? 1 : 0
        }
        ObjRelease(vp)
    }
    if (ro = 1)
        return "no:read-only"
    if (ct = 50004)
        return "edit"
    if (ro = 0)
        return "document"
    if strict
        return "no:a page, not a field"
    focusable := 0
    try ComCall(27, el, "Int*", &focusable)              ; get_CurrentIsKeyboardFocusable
    if (focusable && (tp := SelPattern(el, 10014, "{32eba289-3583-42c9-9c59-3b6d9a1e9b6a}"))) {
        ObjRelease(tp)
        return "document"
    }
    return "no:not a text field"
}

; Put text on the clipboard for one paste, marked so that clipboard history,
; cloud clipboard and clipboard monitors leave it alone.
SelClipboardSet(text) {
    opened := false
    Loop 10 {
        if DllCall("OpenClipboard", "Ptr", A_ScriptHwnd) {
            opened := true
            break
        }
        Sleep(20)
    }
    if !opened
        return false
    ok := false
    try {
        DllCall("EmptyClipboard")
        ok := SelClipData(13, StrPtr(text), (StrLen(text) + 1) * 2)   ; CF_UNICODETEXT
        for name in ["ExcludeClipboardContentFromMonitorProcessing", "CanIncludeInClipboardHistory", "CanUploadToCloudClipboard"]
            SelClipData(DllCall("RegisterClipboardFormat", "Str", name, "UInt"), 0, 4)   ; a zero DWORD
    }
    DllCall("CloseClipboard")
    return ok
}
SelClipData(fmt, src, bytes) {
    h := DllCall("GlobalAlloc", "UInt", 0x42, "UPtr", bytes, "Ptr")   ; GMEM_MOVEABLE | GMEM_ZEROINIT
    if !h
        return false
    p := DllCall("GlobalLock", "Ptr", h, "Ptr")
    if src
        DllCall("RtlMoveMemory", "Ptr", p, "Ptr", src, "UPtr", bytes)
    DllCall("GlobalUnlock", "Ptr", h)
    if DllCall("SetClipboardData", "UInt", fmt, "Ptr", h, "Ptr")
        return true
    DllCall("GlobalFree", "Ptr", h)
    return false
}

; --- UI Automation helpers ---------------------------------------------------
SelUiaGet() {
    global SelUia
    if SelUia
        return SelUia
    ; CUIAutomation8 (Windows 8+, has IUIAutomation2), else CUIAutomation; used as IUIAutomation.
    err := ""
    for clsid in ["{e22ad333-b25f-460c-83d0-0581107395c9}", "{ff48dba4-60ef-4201-aa87-54103eef594e}"] {
        try {
            SelUia := ComObject(clsid, "{30cbe57d-d9d0-452a-ab13-7ac5ac4825ee}")
            break
        } catch as e
            err := e.Message
    }
    if !SelUia {
        SelLog("selection: UI Automation unavailable: " err)
        return 0
    }
    ; A hung app must not hold this script (and so every chord) for UIA's default 20 s.
    ; IUIAutomation2 timeouts; set only when the getters return the documented defaults.
    try {
        u2 := ComObjQuery(SelUia, "{34723aff-0c9d-49d0-9896-7ab52df8cd8a}")
        conn := 0, trans := 0
        ComCall(60, u2, "UInt*", &conn)                  ; get_ConnectionTimeout
        ComCall(62, u2, "UInt*", &trans)                 ; get_TransactionTimeout
        if (conn = 2000 && trans = 20000) {
            ComCall(61, u2, "UInt", 1000)                ; put_ConnectionTimeout
            ComCall(63, u2, "UInt", 1500)                ; put_TransactionTimeout
            SelLog("selection: UI Automation ready (timeouts 1000/1500 ms)")
        } else
            SelLog("selection: UI Automation ready (timeouts left at " conn "/" trans " ms)")
    } catch as e
        SelLog("selection: UI Automation ready (no IUIAutomation2: " e.Message ")")
    return SelUia
}

SelPattern(el, id, iid) {
    static guids := Map()
    if !guids.Has(iid) {
        g := Buffer(16)
        DllCall("ole32\CLSIDFromString", "Str", iid, "Ptr", g, "HRESULT")
        guids[iid] := g
    }
    p := 0
    try ComCall(14, el, "Int", id, "Ptr", guids[iid], "Ptr*", &p)   ; GetCurrentPatternAs
    return p
}

SelPoint(x, y) {
    return (y << 32) | (x & 0xFFFFFFFF)                  ; a POINT passed by value
}

SelExe(win) {
    try return StrLower(WinGetProcessName("ahk_id " win))
    return ""
}

; This script's own windows (launcher, help, OSD): UI Automation must not call into its own thread.
SelOwnWindow(win) {
    try return WinGetPID("ahk_id " win) = DllCall("GetCurrentProcessId", "UInt")
    return false
}
