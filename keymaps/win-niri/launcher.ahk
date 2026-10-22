; ════════════════════════════════════════════════════════════════════════════
; FlowWM application launcher (Mod+Space, Mod+D). Included from flow.ahk.
;
; Fuzzy-finds Start-menu shortcuts, a few built-ins (Settings, Lock, Task
; Manager) and the open top-level windows. Enter runs / activates the
; selection or, with nothing matched, passes the typed text to Run; Esc
; hides. Needs nothing from the daemon (only ShowCentered() and AhkLog()
; from flow.ahk). Keys, index rules and the scoring are in README.md.
; ════════════════════════════════════════════════════════════════════════════

; --- State -----------------------------------------------------------------
LauncherApps := []          ; .lnk + built-in entries, rebuilt lazily (60 s)
LauncherAppsBuilt := 0      ; A_TickCount of the last app scan
LauncherEntries := []       ; windows + apps, rebuilt on every show
LauncherResults := []       ; entries behind the ListBox rows, in row order
LauncherLastQuery := ""     ; raw Edit text the list currently reflects

; --- GUI -------------------------------------------------------------------
LauncherGui := Gui("+AlwaysOnTop -Caption +ToolWindow +Owner", "FlowWM Launcher")
LauncherGui.BackColor := 0x1E1E1E
LauncherGui.MarginX := 12, LauncherGui.MarginY := 12
LauncherGui.SetFont("s14 cE8E8E8", "Segoe UI")
LauncherEdit := LauncherGui.AddEdit("w616 Background2A2A2A")
LauncherGui.SetFont("s11 cDCDCDC", "Segoe UI")
LauncherList := LauncherGui.AddListBox("w616 r10 Background2A2A2A")
LauncherEdit.OnEvent("Change", LauncherFilter)
LauncherList.OnEvent("DoubleClick", LauncherRun)
LauncherGui.OnEvent("Escape", LauncherHide)

; --- Fuzzy matching --------------------------------------------------------
; Case-insensitive subsequence match. 0 = no match, otherwise a score that
; rewards first-character, word-start and consecutive matches and a prefix
; match, and penalises skipped characters; a later word-start occurrence is
; preferred over a mid-word one when the rest of the query still matches.
; The point table and worked examples are in README.md.
FuzzyScore(query, text) {
    q := StrLower(query), t := StrLower(text)
    qlen := StrLen(q), tlen := StrLen(t)
    if (qlen = 0)
        return 1
    if (qlen > tlen || !FuzzySubseq(q, 1, t, 1))
        return 0
    score := 0, ti := 1, prev := 0
    Loop qlen {
        qi := A_Index
        qc := SubStr(q, qi, 1)
        pos := InStr(t, qc, true, ti)           ; always found: subsequence verified above
        if (pos != 1 && pos != prev + 1 && !FuzzyBoundary(text, pos)) {
            alt := pos
            while (alt < tlen) {
                alt := InStr(t, qc, true, alt + 1)
                if !alt
                    break
                if (FuzzyBoundary(text, alt) && FuzzySubseq(q, qi + 1, t, alt + 1)) {
                    pos := alt
                    break
                }
            }
        }
        pts := 10
        if (pos = 1)
            pts += 25
        else if FuzzyBoundary(text, pos)
            pts += 15
        if (prev && pos = prev + 1)
            pts += 12
        pts -= Min(pos - ti, 8)
        score += pts
        prev := pos, ti := pos + 1
    }
    if (SubStr(t, 1, qlen) = q)
        score += 30
    score += Max(0, 10 - tlen // 8)
    return Max(score, 1)
}

; True if q[qFrom..] is a subsequence of t[tFrom..] (both already lower-case).
FuzzySubseq(q, qFrom, t, tFrom) {
    qlen := StrLen(q), tlen := StrLen(t)
    pos := tFrom
    while (qFrom <= qlen) {
        if (pos > tlen)
            return false
        pos := InStr(t, SubStr(q, qFrom, 1), true, pos)
        if !pos
            return false
        pos++, qFrom++
    }
    return true
}

; True if position pos in text starts a "word".
FuzzyBoundary(text, pos) {
    if (pos <= 1)
        return true
    prevCh := SubStr(text, pos - 1, 1)
    curCh := SubStr(text, pos, 1)
    if InStr(" -_./\()[]:,", prevCh)
        return true
    return IsUpper(curCh) && IsLower(prevCh)
}

; Load-time sanity check of the scorer; result goes to AhkLog, never a dialog.
FuzzySelfCheck() {
    bad := ""
    if (FuzzyScore("edge", "Event Viewer") != 0)
        bad .= " edge/EventViewer!=0"
    if (FuzzyScore("edge", "File Explorer") != 0)
        bad .= " edge/FileExplorer!=0"
    if (FuzzyScore("edge", "Registry Editor") != 0)
        bad .= " edge/RegistryEditor!=0"
    if (FuzzyScore("edge", "Microsoft Edge") <= 0)
        bad .= " edge/MicrosoftEdge<=0"
    if (FuzzyScore("eg", "Registry Editor") <= 0)
        bad .= " eg/RegistryEditor<=0"
    if (FuzzyScore("ed", "Registry Editor") <= FuzzyScore("ed", "Recovery Drive"))
        bad .= " ed/RegistryEditor<=RecoveryDrive"
    if (FuzzyScore("fire", "Firefox") <= FuzzyScore("fire", "Configure Firewall"))
        bad .= " fire/Firefox<=ConfigureFirewall"
    if (FuzzyScore("", "anything") != 1)
        bad .= " empty!=1"
    if (FuzzyScore("xyz", "") != 0)
        bad .= " xyz/empty!=0"
    AhkLog(bad ? "fuzzy self-check FAILED:" bad : "fuzzy self-check ok (edge/Microsoft Edge=" FuzzyScore("edge", "Microsoft Edge") ")")
    return bad = ""
}
FuzzySelfCheck()

; --- Index -----------------------------------------------------------------
LauncherBuildIndex() {
    global LauncherApps, LauncherAppsBuilt, LauncherEntries
    if (LauncherApps.Length = 0 || A_TickCount - LauncherAppsBuilt > 60000) {
        LauncherApps := LauncherScanApps()
        LauncherAppsBuilt := A_TickCount
    }
    LauncherEntries := []
    for e in LauncherWindows()
        LauncherEntries.Push(e)
    for e in LauncherApps
        LauncherEntries.Push(e)
}

; Every .lnk under the Start menus, deduplicated by display name
; (case-insensitive); "Uninstall*" skipped; .url ignored by the pattern.
LauncherScanApps() {
    apps := []
    seen := Map()
    done := []
    for root in [A_StartMenu, A_StartMenuCommon, A_Programs, A_ProgramsCommon] {
        if (root = "" || !DirExist(root))
            continue
        covered := false
        for d in done {
            if (root = d || SubStr(root, 1, StrLen(d) + 1) = d "\")
                covered := true
        }
        if covered
            continue
        done.Push(root)
        Loop Files, root "\*.lnk", "FR" {
            name := SubStr(A_LoopFileName, 1, -4)
            if (name = "" || SubStr(name, 1, 9) = "Uninstall")
                continue
            key := StrLower(name)
            if seen.Has(key)
                continue
            seen[key] := true
            apps.Push({name: name, kind: "App", run: A_LoopFileFullPath})
        }
    }
    apps.Push({name: "Settings", kind: "Setting", run: "ms-settings:"})
    apps.Push({name: "Lock", kind: "Setting", run: "rundll32.exe user32.dll,LockWorkStation"})
    apps.Push({name: "Task Manager", kind: "Setting", run: "taskmgr.exe"})
    return apps
}

; Open top-level windows: skip empty titles, tool windows, cloaked windows
; (other virtual desktops, suspended UWP), the shell's own frames and every
; window of this script.
LauncherWindows() {
    out := []
    me := ProcessExist()
    for hwnd in WinGetList() {
        try {
            if (WinGetPID(hwnd) = me)
                continue
            title := WinGetTitle(hwnd)
            if (title = "")
                continue
            if (WinGetExStyle(hwnd) & 0x80)      ; WS_EX_TOOLWINDOW
                continue
            cls := WinGetClass(hwnd)
            if (cls = "Progman" || cls = "WorkerW" || cls = "Shell_TrayWnd" || cls = "Windows.UI.Core.CoreWindow")
                continue
            if LauncherCloaked(hwnd)
                continue
            proc := WinGetProcessName(hwnd)
            title := StrReplace(StrReplace(title, "`r", " "), "`n", " ")
            out.Push({name: title, kind: "Window", sub: proc, hwnd: hwnd})
        }
    }
    return out
}

LauncherCloaked(hwnd) {
    cloaked := 0
    DllCall("dwmapi\DwmGetWindowAttribute", "Ptr", hwnd, "UInt", 14, "UInt*", &cloaked, "UInt", 4)
    return cloaked != 0
}

; Row text: "name  —  kind"; window titles longer than 70 chars are cut with
; an ellipsis (the full title is still what gets matched).
LauncherDisplay(e) {
    sep := "  " Chr(0x2014) "  "
    name := e.name
    if (StrLen(name) > 70)
        name := RTrim(SubStr(name, 1, 69)) Chr(0x2026)
    if (e.kind = "Window")
        return name "  (" e.sub ")" sep "Window"
    return name sep e.kind
}

; --- Behaviour -------------------------------------------------------------
LauncherToggle(*) {
    if DllCall("IsWindowVisible", "Ptr", LauncherGui.Hwnd) {
        LauncherHide()
        return
    }
    LauncherBuildIndex()
    LauncherEdit.Value := ""
    LauncherFilter()
    ShowCentered(LauncherGui)
    LauncherEdit.Focus()
    SetTimer(LauncherWatch, 100)
}

LauncherHide(*) {
    SetTimer(LauncherWatch, 0)
    LauncherGui.Hide()
}

; Backstop while the launcher is visible: if the Edit text no longer matches
; what the list was built from (a missed Change event), refilter; and keep
; the keyboard focus on the Edit so typed letters never go to the ListBox
; (whose own type-ahead would otherwise eat them).
LauncherWatch() {
    if !DllCall("IsWindowVisible", "Ptr", LauncherGui.Hwnd) {
        SetTimer(LauncherWatch, 0)
        return
    }
    if (LauncherEdit.Value !== LauncherLastQuery)
        LauncherFilter()
    try {
        if (LauncherGui.FocusedCtrl.Hwnd = LauncherList.Hwnd)
            LauncherEdit.Focus()
    }
}

; Re-score every entry against the query and refill the list. Sorting is
; done by Sort() on "paddedScore<TAB>name<TAB>index" lines: fixed-width
; zero-padded (999999 - score), so plain lexicographic order gives score
; descending, then name (Sort is case-insensitive). The entry index is read
; back from the end of each sorted line. Entries scoring 0 are dropped.
LauncherFilter(*) {
    global LauncherResults, LauncherLastQuery
    raw := LauncherEdit.Value
    LauncherLastQuery := raw
    q := Trim(raw)
    lines := ""
    for i, e in LauncherEntries {
        if (q = "")
            s := (e.kind = "Window") ? 2 : 1       ; empty query: windows first
        else {
            s := FuzzyScore(q, e.name)
            if (e.kind = "Window") {
                s2 := FuzzyScore(q, e.sub)
                if (s2 > s)
                    s := s2
            }
        }
        if (s > 0)
            lines .= Format("{:06}", 999999 - s) "`t" e.name "`t" i "`n"
    }
    LauncherResults := []
    items := []
    if (lines != "") {
        Loop Parse, Sort(RTrim(lines, "`n")), "`n" {
            if (items.Length >= 60)
                break
            idx := Integer(SubStr(A_LoopField, InStr(A_LoopField, "`t", , -1) + 1))
            e := LauncherEntries[idx]
            LauncherResults.Push(e)
            items.Push(LauncherDisplay(e))
        }
    }
    LauncherList.Delete()
    if (items.Length > 0) {
        LauncherList.Add(items)
        LauncherList.Choose(1)
    }
    AhkLog("launcher filter '" q "' -> " items.Length " row(s)" (items.Length ? ", top: " LauncherResults[1].name : ""))
}

LauncherMove(delta) {
    n := LauncherResults.Length
    if (n = 0)
        return
    i := LauncherList.Value + delta
    i := Max(1, Min(n, i))
    LauncherList.Choose(i)
}

LauncherRun(*) {
    if (LauncherEdit.Value !== LauncherLastQuery)   ; never act on a stale list
        LauncherFilter()
    i := LauncherList.Value
    text := Trim(LauncherEdit.Value)
    if (i < 1 && LauncherResults.Length > 0)
        i := 1
    LauncherHide()
    if (i >= 1 && i <= LauncherResults.Length) {
        LauncherActivate(LauncherResults[i])
        return
    }
    if (text = "")
        return
    try Run(text, EnvGet("USERPROFILE"))
    catch as err
        Osd("could not run " text ": " err.Message)
}

LauncherActivate(e) {
    try {
        if (e.kind = "Window")
            WinActivate("ahk_id " e.hwnd)
        else
            Run(e.run, EnvGet("USERPROFILE"))
    } catch as err
        Osd("could not open " e.name ": " err.Message)
}

; --- Hotkeys ---------------------------------------------------------------
#HotIf DaemonUp
#Space::LauncherToggle()
#d::LauncherToggle()
#HotIf

#HotIf IsSet(LauncherGui) && WinActive("ahk_id " LauncherGui.Hwnd)
Up::LauncherMove(-1)
Down::LauncherMove(1)
PgUp::LauncherMove(-10)
PgDn::LauncherMove(10)
Tab::LauncherMove(1)
+Tab::LauncherMove(-1)
Enter::LauncherRun()
NumpadEnter::LauncherRun()
Escape::LauncherHide()
#HotIf
