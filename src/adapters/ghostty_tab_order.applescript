on run
  tell application "Ghostty"
    set positions to {}
    set windowOrdinal to 0
    repeat with ghosttyWindow in windows
      set windowOrdinal to windowOrdinal + 1
      repeat with ghosttyTab in tabs of ghosttyWindow
        repeat with ghosttyTerminal in terminals of ghosttyTab
          set end of positions to (windowOrdinal as text) & (character id 9) & ((index of ghosttyTab) as text) & (character id 9) & (tty of ghosttyTerminal)
        end repeat
      end repeat
    end repeat
    set AppleScript's text item delimiters to linefeed
    return positions as text
  end tell
end run
