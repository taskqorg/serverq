//! One explicit sshhole diagnostic. No login attempt, workload restart or remote command channel.
use serde_json::{Value,json};

const PROBE:&str=r#"
musl=0; glibc=0; sshd=0; fixed=0; password=2; pam=2; stars=2; hashes=2; conditional=2
for p in /lib/ld-musl-*.so.1; do [ -f "$p" ] && musl=1; done
for p in /lib/x86_64-linux-gnu/libc.so.6 /lib/aarch64-linux-gnu/libc.so.6 /lib64/libc.so.6; do [ -f "$p" ] && glibc=1; done
binary=
for p in /usr/sbin/sshd /sbin/sshd /usr/local/sbin/sshd; do [ -x "$p" ] && binary=$p && break; done
if [ -n "$binary" ]; then
 sshd=1
 version=$("$binary" -V 2>&1 | sed -n 's/^OpenSSH_\([0-9][0-9]*\)\.\([0-9][0-9]*\)p\([0-9][0-9]*\).*/\1 \2 \3/p' | head -n 1)
 set -- $version
 if [ "$#" = 3 ]; then
  if [ "$1" -gt 10 ] || { [ "$1" -eq 10 ] && [ "$2" -gt 6 ]; } || { [ "$1" -eq 10 ] && [ "$2" -eq 6 ] && [ "$3" -ge 2 ]; }; then fixed=1; fi
 fi
 cfg=$("$binary" -T 2>/dev/null) && {
  password=$(printf '%s\n' "$cfg" | awk '$1=="passwordauthentication"{print ($2=="yes"?1:0)}')
  pam=$(printf '%s\n' "$cfg" | awk '$1=="usepam"{print ($2=="yes"?1:0)}')
 }
 if [ -r /etc/shadow ]; then
  stars=$(awk -F: 'BEGIN{n=0}$2=="*"{n++}END{print(n>0?1:0)}' /etc/shadow)
  hashes=$(awk -F: 'BEGIN{n=0}$2~/^\$[56]\$/{n++}END{print(n>0?1:0)}' /etc/shadow)
 fi
 if [ -r /etc/ssh/sshd_config ]; then
  conditional=0
  grep -Eq '^[[:space:]]*(Match|Include)[[:space:]]' /etc/ssh/sshd_config && conditional=1
  for p in /etc/ssh/sshd_config.d/*.conf; do [ -r "$p" ] && grep -Eq '^[[:space:]]*(Match|Include)[[:space:]]' "$p" && conditional=1; done
 fi
fi
printf 'musl=%s\nglibc=%s\nsshd=%s\nfixed=%s\npassword=%s\npam=%s\nstars=%s\nhashes=%s\nconditional=%s\n' "$musl" "$glibc" "$sshd" "$fixed" "$password" "$pam" "$stars" "$hashes" "$conditional"
"#;

fn classify(text:&str)->(&'static str,&'static str) {
    let mut fields=std::collections::BTreeMap::new();
    for line in text.lines() {
        let Some((key,value))=line.split_once('=') else{return ("unknown","invalid-probe-result");};
        if !["musl","glibc","sshd","fixed","password","pam","stars","hashes","conditional"].contains(&key)||!["0","1","2"].contains(&value)||fields.insert(key,value).is_some(){return ("unknown","invalid-probe-result");}
    }
    if fields.len()!=9 {return ("unknown","incomplete-probe-result");}
    let is=|key:&str,value:&str|fields.get(key)==Some(&value);
    if is("glibc","1")&&is("musl","0") {return ("not-affected","glibc");}
    if is("fixed","1") {return ("not-affected","upstream-fix-present");}
    let conditional=is("conditional","1")||is("conditional","2");
    if is("musl","1")&&is("sshd","1")&&is("password","1")&&is("pam","0")&&is("stars","1")&&is("hashes","1") {
        return ("potentially-affected",if conditional {"default-conditions-present-context-unverified"}else{"exposure-conditions-present-fix-unverified"});
    }
    if conditional {return ("unknown","conditional-or-unreadable-config");}
    if is("password","0") {return ("not-affected","password-authentication-disabled");}
    if is("pam","1") {return ("not-affected","pam-enabled");}
    // Missing account classes can change independently; retain the observations and avoid an unsupported safety claim.
    ("unknown","conditions-incomplete-or-fix-unverified")
}

fn finding(sample:&mut Value,target:(&str,&str),status:&str,reason:&str) {
    let name=format!("sshhole:{}",crate::sha256(json!(target).to_string().as_bytes()));
    let rows=sample["entities"].as_array_mut().unwrap();
    rows.push(json!({"entity":"finding","identity":name,"status":status,"reason":reason}));
    crate::discovery::link(rows,"checks",("finding",&name),target);
    sample["security"]["sshhole"].as_array_mut().unwrap().push(json!({"target_entity":target.0,"target_identity":target.1,"status":status,"reason":reason}));
}

pub fn collect(sample:&mut Value) {
    sample["security"]=json!({"sshhole":[]});
    if cfg!(target_os="linux") {
        let result=crate::command(&["sh","-c",PROBE]);
        let (status,reason)=result.as_deref().map(classify).unwrap_or(("unknown","probe-unavailable"));
        finding(sample,("host","host"),status,reason);
    }else{finding(sample,("host","host"),"not-affected","native-platform-not-musl-linux");}
    for runtime in crate::discovery::runtimes() {
        match crate::discovery::containers(&runtime) {
            Ok(containers)=>for container in containers {
                let name=crate::discovery::container_name(&runtime,&container);
                let result=if container["status"]=="running" {
                    if let Some(id)=container["id"].as_str().filter(|s|s.len()==64&&s.bytes().all(|c|c.is_ascii_hexdigit())) {
                        crate::discovery::probe(&runtime,&["container","exec",id,"sh","-c",PROBE])
                    }else{Err("invalid-container-identity".into())}
                }else{Err("container-not-running".into())};
                let (status,reason)=result.as_deref().map(classify).unwrap_or(("unknown",if container["status"]=="running" {"probe-unavailable"}else{"container-not-running"}));
                finding(sample,("container",&name),status,reason);
            },
            Err(_)=>finding(sample,("service",&format!("{}:runtime",crate::discovery::runtime_label(&runtime))),"unknown","runtime-unavailable"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn sshhole_reports_conditions_without_claiming_a_backport_or_leaking_hashes() {
        let exposed="musl=1\nglibc=0\nsshd=1\nfixed=0\npassword=1\npam=0\nstars=1\nhashes=1\nconditional=0\n";
        assert_eq!(classify(exposed).0,"potentially-affected");
        for (before,after) in [("fixed=0","fixed=1"),("password=1","password=0"),("pam=0","pam=1")] {assert_eq!(classify(&exposed.replace(before,after)).0,"not-affected");}
        assert_eq!(classify(&exposed.replace("conditional=0","conditional=1")).0,"potentially-affected");
        assert_eq!(classify(&exposed.replace("conditional=0","conditional=1").replace("password=1","password=0")).0,"unknown");
        assert_eq!(classify(&exposed.replace("stars=1","stars=2")).0,"unknown");
        assert_eq!(classify(&(exposed.to_owned()+"password=0\n")).0,"unknown");
        assert_eq!(classify("invented-secret").0,"unknown");
    }
}
