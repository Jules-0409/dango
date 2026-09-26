// 只被 macOS 的 `security` 和 Windows 的凭据管理器两条路径用到。
// Linux 上两个函数都被 cfg 掉，不加这道 cfg 就是 unused import，
// clippy -D warnings 会直接失败（CI 上就是这么红过一次）。
#[cfg(any(target_os = "macos", target_os = "windows"))]
use base64::prelude::*;
#[cfg(any(target_os = "macos", target_os = "windows"))]
use std::process::Command;

/// Read password from macOS Keychain using `security` CLI
#[cfg(target_os = "macos")]
fn read_macos_keychain(service: &str, account: &str) -> Option<String> {
    let output = Command::new("security")
        .args(["find-generic-password", "-s", service, "-a", account, "-w"])
        .output()
        .ok()?;

    if output.status.success() {
        let val = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if !val.is_empty() {
            return Some(val);
        }
    }
    None
}

/// Read credential from Windows Credential Manager using PowerShell
#[cfg(target_os = "windows")]
fn read_windows_credential(target: &str) -> Option<Vec<u8>> {
    let ps_cmd = format!(
        r#"$sig = @"
using System;
using System.Runtime.InteropServices;
public class QPCred {{
  [DllImport("advapi32.dll", SetLastError=true, CharSet=CharSet.Unicode)]
  public static extern bool CredRead(string target, int type, int flags, out IntPtr credential);
  [DllImport("advapi32.dll")]
  public static extern void CredFree(IntPtr cred);
  [StructLayout(LayoutKind.Sequential)]
  public struct CREDENTIAL {{
    public int Flags; public int Type; public IntPtr TargetName; public IntPtr Comment;
    public long LastWritten; public int CredentialBlobSize; public IntPtr CredentialBlob;
    public int Persist; public int AttributeCount; public IntPtr Attributes;
    public IntPtr TargetAlias; public IntPtr UserName;
  }}
  public static byte[] Get(string target) {{
    IntPtr p;
    if (!CredRead(target, 1, 0, out p)) return null;
    CREDENTIAL cr = (CREDENTIAL)Marshal.PtrToStructure(p, typeof(CREDENTIAL));
    byte[] b = new byte[cr.CredentialBlobSize];
    Marshal.Copy(cr.CredentialBlob, b, 0, cr.CredentialBlobSize);
    CredFree(p);
    return b;
  }}
}}
"@
Add-Type -TypeDefinition $sig
$b = [QPCred]::Get("{}")
if ($b -ne $null) {{ [Convert]::ToBase64String($b) }}
"#,
        target
    );

    for exe in ["powershell", "pwsh"] {
        if let Ok(output) = Command::new(exe)
            .args(["-NoProfile", "-NonInteractive", "-Command", &ps_cmd])
            .output()
        {
            if output.status.success() {
                let out_str = String::from_utf8_lossy(&output.stdout).trim().to_string();
                if let Ok(decoded) = BASE64_STANDARD.decode(out_str.as_bytes()) {
                    return Some(decoded);
                }
            }
        }
    }
    None
}

/// Retrieve the Factory 32-byte AES key from Keychain or Windows Credential Manager
pub fn load_factory_key() -> Result<Vec<u8>, String> {
    #[cfg(target_os = "macos")]
    let service = "Factory CLI";

    #[cfg(target_os = "macos")]
    {
        let accounts = ["auth-encryption-key-security-cli", "auth-encryption-key"];
        for account in accounts {
            if let Some(val) = read_macos_keychain(service, account) {
                if let Ok(key) = BASE64_STANDARD.decode(val.as_bytes()) {
                    if key.len() == 32 {
                        return Ok(key);
                    }
                }
            }
        }
        Err("Cannot find 32-byte Factory key in macOS Keychain".into())
    }

    #[cfg(target_os = "windows")]
    {
        let targets = [
            "Factory CLI/auth-encryption-key",
            "Factory CLI/auth-encryption-key-security-cli",
        ];
        for target in targets {
            if let Some(blob) = read_windows_credential(target) {
                // Blob could be direct 32 bytes or base64 utf8 string
                if blob.len() == 32 {
                    return Ok(blob);
                }
                if let Ok(s) = String::from_utf8(blob) {
                    if let Ok(key) = BASE64_STANDARD.decode(s.trim().as_bytes()) {
                        if key.len() == 32 {
                            return Ok(key);
                        }
                    }
                }
            }
        }
        Err("Cannot find 32-byte Factory key in Windows Credential Manager".into())
    }

    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        Err("Unsupported operating system for keyring retrieval".into())
    }
}
