use std::fs;
use std::net::TcpListener;
use std::process::Command;

#[test]
fn configuration_check_validates_without_binding_or_exposing_secrets() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let directory = std::env::temp_dir().join(format!("brume-check-{}", std::process::id()));
    fs::create_dir(&directory).unwrap();
    let path = directory.join("brume.conf");
    let secret = "校验密码不得出现在输出中";
    fs::write(
        &path,
        format!(
            "port={}\nusername=admin\npassword={secret}\nwhitelist=127.0.0.1\n",
            listener.local_addr().unwrap().port()
        ),
    )
    .unwrap();
    // 端口已被占用，配置校验仍应成功，且参数顺序不影响模式识别。
    for prefix in [true, false] {
        let mut command = Command::new(env!("CARGO_BIN_EXE_brume"));
        if prefix {
            command.arg("--check-config");
        }
        command.arg("--config").arg(&path);
        if !prefix {
            command.arg("--check-config");
        }
        let output = command.output().unwrap();
        assert!(output.status.success(), "{output:?}");
        assert!(!String::from_utf8_lossy(&output.stdout).contains(secret));
        assert!(!String::from_utf8_lossy(&output.stderr).contains(secret));
    }
    for contents in [
        "whitelist=不是有效地址\n",
        "username=admin\n",
        "udp_timeout=18446744073709551615\n",
    ] {
        fs::write(&path, contents).unwrap();
        let output = Command::new(env!("CARGO_BIN_EXE_brume"))
            .arg("--config")
            .arg(&path)
            .arg("--check-config")
            .output()
            .unwrap();
        assert!(!output.status.success());
    }
    fs::remove_file(path).unwrap();
    fs::remove_dir(directory).unwrap();
}
