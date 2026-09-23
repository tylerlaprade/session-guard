on run {workingDirectory, initialInput, restoreWindowId}
  tell application "Ghostty"
    set cfg to new surface configuration
    set initial working directory of cfg to workingDirectory
    set initial input of cfg to initialInput
    set restoreWindow to missing value
    repeat with candidate in windows
      if id of candidate is restoreWindowId then set restoreWindow to contents of candidate
    end repeat
    if restoreWindow is missing value then
      if (count of windows) is 0 then
        set restoreWindow to new window with configuration cfg
        return (id of restoreWindow) & (character id 9) & (id of selected tab of restoreWindow)
      end if
      set restoreWindow to front window
    end if
    set newTab to new tab in restoreWindow with configuration cfg
    return (id of restoreWindow) & (character id 9) & (id of newTab)
  end tell
end run
