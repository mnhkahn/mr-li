on run argv
    set operation to item 1 of argv
    set parameter to item 2 of argv

    if operation is in {"status", "pause", "next", "prev"} then
        if application id "com.apple.Music" is not running then
            return "音乐 App 尚未打开。用 /music play 开始播放。"
        end if
    end if

    with timeout of 12 seconds
        tell application id "com.apple.Music"
            if operation is "open" then
                open location parameter
                return "已请求音乐 App 打开歌曲页，可在页面点播放。"
            end if

            if operation is in {"list", "find"} then
                if not (exists library playlist 1) then return "音乐资料库尚未就绪。"
                set librarySongs to library playlist 1
                if operation is "list" then
                    set firstIndex to ((parameter as integer) - 1) * 15 + 1
                    set songCount to count tracks of librarySongs
                    if firstIndex > songCount then return "此页没有歌曲。"
                    set lastIndex to firstIndex + 14
                    if lastIndex > songCount then set lastIndex to songCount
                    set songRows to get tracks firstIndex thru lastIndex of librarySongs
                    set heading to "音乐资料库 · 第 " & parameter & " 页（共 " & songCount & " 首）"
                else
                    set songRows to search librarySongs for parameter
                    if (count songRows) is 0 then return "资料库没有匹配歌曲。"
                    set heading to "资料库搜索结果（最多显示 15 首）"
                end if
                set output to heading
                repeat with rowIndex from 1 to my minOf(15, count songRows)
                    set songRow to item rowIndex of songRows
                    set songID to persistent ID of songRow
                    set output to output & linefeed & "• " & my trackLabel(songRow) & " [" & songID & "]"
                end repeat
                return output
            end if

            if operation is "play" then
                if parameter is "" then
                    play
                else
                    set selectedSong to first track of library playlist 1 whose persistent ID is parameter
                    play selectedSong
                end if
                repeat 12 times
                    if player state is playing then
                        if parameter is "" then exit repeat
                        try
                            if persistent ID of current track is parameter then exit repeat
                        end try
                    end if
                    delay 0.25
                end repeat
                if player state is not playing then error "音乐尚未开始播放。请检查资料库、账号或音乐 App 当前提示。"
                if parameter is not "" then
                    if persistent ID of current track is not parameter then error "音乐 App 尚未切换到指定歌曲。"
                end if
            else if operation is "pause" then
                pause
            else if operation is "next" then
                if player state is stopped then return "当前没有正在播放的歌曲。"
                next track
                delay 0.25
            else if operation is "prev" then
                if player state is stopped then return "当前没有正在播放的歌曲。"
                previous track
                delay 0.25
            end if

            set playbackState to player state
            if playbackState is playing then
                set labelText to "正在播放"
            else if playbackState is paused then
                set labelText to "已暂停"
            else if playbackState is stopped then
                set labelText to "已停止"
            else
                set labelText to "播放状态：" & (playbackState as text)
            end if
            -- Catalog streams may have no scriptable track object.
            try
                if playbackState is not stopped then set labelText to labelText & " · " & my trackLabel(current track)
            end try
            return labelText
        end tell
    end timeout
end run

on trackLabel(songRow)
    tell application id "com.apple.Music" to set {titleText, artistText} to {name of songRow, artist of songRow}
    return titleText & " — " & artistText
end trackLabel

on minOf(leftValue, rightValue)
    if leftValue < rightValue then return leftValue
    return rightValue
end minOf
