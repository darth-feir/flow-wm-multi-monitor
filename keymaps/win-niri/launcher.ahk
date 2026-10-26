; ════════════════════════════════════════════════════════════════════════════
; FlowWM application launcher (the `launcher` action: Mod+Space, Mod+D by
; default). Included from flow.ahk.
;
; Fuzzy-finds one kind of thing at a time, chosen by a prefix on the query:
; "app - x" (Start-menu shortcuts and a few built-ins: Settings, Lock, Task
; Manager), "window - x" (open top-level windows) or "file - x" (files
; under the user's folders and the shell's Recent items). No prefix means
; app. Enter runs / activates the selection or, with nothing matched,
; passes the typed text to Run; Esc hides. Needs nothing from the daemon
; (only ShowCentered() and AhkLog() from flow.ahk). Keys, index rules and
; the scoring are in README.md.
; ════════════════════════════════════════════════════════════════════════════

; --- State -----------------------------------------------------------------
LauncherApps := []          ; .lnk + built-in entries, rebuilt lazily (60 s)
LauncherAppsBuilt := 0      ; A_TickCount of the last app scan
LauncherWins := []          ; open windows, rebuilt on every show
LauncherFiles := []         ; file entries, built on the first "file - " query, kept 60 s
LauncherFilesBuilt := 0     ; A_TickCount of the last file scan
LauncherResults := []       ; entries behind the ListBox rows, in row order
LauncherLastQuery := ""     ; raw Edit text the list currently reflects

; Where "file - x" looks: these folders, LauncherFileDepth levels deep, at
; most LauncherFileCap files in all, plus the shell's Recent items.
LauncherFileRoots := [A_Desktop, A_MyDocuments, EnvGet("USERPROFILE") "\Downloads", EnvGet("USERPROFILE") "\Pictures", EnvGet("USERPROFILE") "\Videos", EnvGet("USERPROFILE") "\Music"]
LauncherFileDepth := 3
LauncherFileCap := 5000

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

; --- Query -----------------------------------------------------------------
; "app - x" / "window - x" / "file - x" (any case, spaces around the dash
; optional) selects the kind searched; anything else is an app query.
LauncherParse(q) {
    if RegExMatch(q, "i)^(app|window|file)\s*-\s*(.*)$", &m)
        return {kind: StrLower(m[1]), text: Trim(m[2])}
    return {kind: "app", text: q}
}

; --- Index -----------------------------------------------------------------
; Called on every show: windows are re-enumerated, apps every 60 s. Files
; are scanned by LauncherFilter the first time a "file - " query asks.
LauncherBuildIndex() {
    global LauncherApps, LauncherAppsBuilt, LauncherWins
    if (LauncherApps.Length = 0 || A_TickCount - LauncherAppsBuilt > 60000) {
        LauncherApps := LauncherScanApps()
        LauncherAppsBuilt := A_TickCount
    }
    LauncherWins := LauncherWindows()
}

; The entries a query of the given kind is matched against.
LauncherEntriesFor(kind) {
    global LauncherFiles, LauncherFilesBuilt
    if (kind = "window")
        return LauncherWins
    if (kind = "file") {
        if (LauncherFilesBuilt = 0 || A_TickCount - LauncherFilesBuilt > 60000) {
            t0 := A_TickCount
            LauncherFiles := LauncherScanFiles()
            LauncherFilesBuilt := A_TickCount
            AhkLog("launcher file scan: " LauncherFiles.Length " file(s) in " (A_TickCount - t0) " ms")
        }
        return LauncherFiles
    }
    return LauncherApps
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

; Files under LauncherFileRoots (LauncherFileDepth levels, hidden and
; system entries and dot-folders skipped, LauncherFileCap in all) plus the
; targets of the shell's Recent shortcuts that still exist and are not
; folders. Deduplicated by full path, case-insensitively.
LauncherScanFiles() {
    files := []
    seen := Map()
    for root in LauncherFileRoots {
        if (files.Length >= LauncherFileCap)
            break
        if (root = "" || !DirExist(root))
            continue
        LauncherScanDir(root, LauncherFileDepth, files, seen)
    }
    recent := EnvGet("APPDATA") "\Microsoft\Windows\Recent"
    if DirExist(recent) {
        Loop Files, recent "\*.lnk" {
            target := ""
            try FileGetShortcut(A_LoopFileFullPath, &target)
            catch
                continue
            attr := (target = "") ? "" : FileExist(target)
            if (attr = "" || InStr(attr, "D") || seen.Has(StrLower(target)))
                continue
            seen[StrLower(target)] := true
            SplitPath(target, &fname, &fdir)
            files.Push({name: fname, kind: "File", sub: fdir, run: target})
        }
    }
    return files
}

LauncherScanDir(dir, depth, files, seen) {
    Loop Files, dir "\*", "FD" {
        if (files.Length >= LauncherFileCap)
            return
        if (InStr(A_LoopFileAttrib, "H") || InStr(A_LoopFileAttrib, "S") || SubStr(A_LoopFileName, 1, 1) = ".")
            continue
        if InStr(A_LoopFileAttrib, "D") {
            if (depth > 1)
                LauncherScanDir(A_LoopFileFullPath, depth - 1, files, seen)
            continue
        }
        key := StrLower(A_LoopFileFullPath)
        if seen.Has(key)
            continue
        seen[key] := true
        files.Push({name: A_LoopFileName, kind: "File", sub: A_LoopFileDir, run: A_LoopFileFullPath})
    }
}

; Row text: "name  —  kind"; names longer than 70 chars are cut with an
; ellipsis (the full name is still what gets matched). Windows show their
; process, files the tail of their folder.
LauncherDisplay(e) {
    sep := "  " Chr(0x2014) "  "
    name := e.name
    if (StrLen(name) > 70)
        name := RTrim(SubStr(name, 1, 69)) Chr(0x2026)
    if (e.kind = "Window")
        return name "  (" e.sub ")" sep "Window"
    if (e.kind = "File") {
        dir := e.sub
        if (StrLen(dir) > 40)
            dir := Chr(0x2026) SubStr(dir, -39)
        return name "  (" dir ")" sep "File"
    }
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
    AhkLog("launcher hidden")
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

; Re-score every entry of the query's kind against its text and refill the
; list. Sorting is done by Sort() on "paddedScore<TAB>name<TAB>index"
; lines: fixed-width zero-padded (999999 - score), so plain lexicographic
; order gives score descending, then name (Sort is case-insensitive). The
; entry index is read back from the end of each sorted line. Entries
; scoring 0 are dropped; an empty text lists the whole kind by name.
LauncherFilter(*) {
    global LauncherResults, LauncherLastQuery
    Critical "On"             ; a later Change event must not interleave with this one (README.md)
    raw := LauncherEdit.Value
    LauncherLastQuery := raw
    p := LauncherParse(Trim(raw))
    q := p.text
    entries := LauncherEntriesFor(p.kind)
    lines := ""
    for i, e in entries {
        if (q = "")
            s := 1
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
            e := entries[idx]
            LauncherResults.Push(e)
            items.Push(LauncherDisplay(e))
        }
    }
    LauncherList.Delete()
    if (items.Length > 0) {
        LauncherList.Add(items)
        LauncherList.Choose(1)
    }
    AhkLog("launcher filter " p.kind " '" q "' -> " items.Length " row(s)" (items.Length ? ", top: " LauncherResults[1].name : ""))
    Critical "Off"
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
    text := LauncherParse(Trim(LauncherEdit.Value)).text
    if (i < 1 && LauncherResults.Length > 0)
        i := 1
    LauncherHide()
    if (i >= 1 && i <= LauncherResults.Length) {
        AhkLog("launcher run row " i ": " LauncherResults[i].kind " " LauncherResults[i].name)
        LauncherActivate(LauncherResults[i])
        return
    }
    if (text = "")
        return
    AhkLog("launcher run text: " text)
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
; The chords that open the launcher (Mod+Space and Mod+D by default) are the
; `launcher` action in the binds table; flow.ahk registers them. Only the
; keys inside the panel are bound here.
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
