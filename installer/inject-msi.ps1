# PowerShell script to inject feather.exe into MSI cabinet
# Requires: WiX v4 built MSI (structure only), feather.exe

 = "..\target\installer\Feather-1.0.0.msi"
 = "..\target\release\feather.exe"
 = [IO.Path]::GetTempPath() + "msi_inject_" + [Guid]::NewGuid()
New-Item -ItemType Directory -Path  | Out-Null

try {
    # Extract MSI to temp directory
     = New-Object -ComObject WindowsInstaller.Installer
     = .OpenDatabase(, 1)  # 1 = msiOpenDatabaseModeTransact
    
    # Add file to Media table (cabinet)
    # This is complex - need to use Cabinet Builder API
    # Simpler: use WiX's inscribe or candle/light from v3
    
    Write-Host "MSI database opened. Manual cabinet injection needed."
    Write-Host "Consider using WiX v3 candle/light or Inno Setup instead."
} finally {
    Remove-Item -Recurse -Force  -ErrorAction SilentlyContinue
}
