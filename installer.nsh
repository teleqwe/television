; Uninstalling also removes "Start with Windows", which Television sets itself.
!macro NSIS_HOOK_POSTUNINSTALL
  DeleteRegValue HKCU "Software\Microsoft\Windows\CurrentVersion\Run" "Television"
!macroend
