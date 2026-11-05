# Run on Windows PowerShell 5.1 without downloading or installing anything.
Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$installer = Join-Path $PSScriptRoot 'install.ps1'
$tokens = $null
$parseErrors = $null
$ast = [System.Management.Automation.Language.Parser]::ParseFile(
    $installer, [ref]$tokens, [ref]$parseErrors
)
if ($parseErrors.Count -ne 0) {
    throw "Installer has PowerShell syntax errors: $($parseErrors[0])"
}
$functions = @($ast.FindAll({
    param($node)
    $node -is [System.Management.Automation.Language.FunctionDefinitionAst] -and
        $node.Name -eq 'Test-NovaWindowsX64'
}, $true))
if ($functions.Count -ne 1) {
    throw 'Expected exactly one Test-NovaWindowsX64 function in the installer.'
}

# Extract just the pure architecture predicate: executing install.ps1 downloads a release.
Invoke-Expression $functions[0].Extent.Text

$cases = @(
    @{ Name = 'native x64'; Is64BitOS = $true; Process = 'AMD64'; Wow64 = ''; Expected = $true },
    @{ Name = '32-bit PowerShell on x64'; Is64BitOS = $true; Process = 'x86'; Wow64 = 'AMD64'; Expected = $true },
    @{ Name = '32-bit Windows'; Is64BitOS = $false; Process = 'x86'; Wow64 = ''; Expected = $false },
    @{ Name = 'native ARM64'; Is64BitOS = $true; Process = 'ARM64'; Wow64 = ''; Expected = $false },
    @{ Name = '32-bit PowerShell on ARM64'; Is64BitOS = $true; Process = 'x86'; Wow64 = 'ARM64'; Expected = $false },
    @{ Name = 'x64 emulation on ARM64'; Is64BitOS = $true; Process = 'AMD64'; Wow64 = 'ARM64'; Expected = $false },
    @{ Name = 'missing architecture'; Is64BitOS = $true; Process = ''; Wow64 = ''; Expected = $false }
)
foreach ($case in $cases) {
    $actual = Test-NovaWindowsX64 -Is64BitOS $case.Is64BitOS `
        -ProcessArchitecture $case.Process -Wow64Architecture $case.Wow64
    if ($actual -ne $case.Expected) {
        throw "Architecture check failed: $($case.Name). Expected $($case.Expected), got $actual."
    }
}
Write-Host "Passed $($cases.Count) Windows installer architecture cases."
