//! Facts and typed links, without a project manifest or raw configuration in observations.
use serde_json::{Value,json};
use std::{fs,path::{Path,PathBuf}};

pub fn link(rows:&mut Vec<Value>,relation:&str,from:(&str,&str),to:(&str,&str)) {
    let identity=crate::sha256(json!([relation,from,to]).to_string().as_bytes());
    rows.push(json!({"entity":"link","identity":identity,"relation":relation,"from_entity":from.0,"from_identity":from.1,"to_entity":to.0,"to_identity":to.1}));
}
fn simple(rows:&mut Vec<Value>,entity:&str,identity:&str,status:&str) {
    rows.push(json!({"entity":entity,"identity":identity,"status":status}));
}

pub fn runtimes()->Vec<Vec<String>> {
    let mut out=vec![];
    #[cfg(target_os="linux")] {
        if Path::new("/var/run/docker.sock").exists() {out.push(vec!["docker".into(),"--host".into(),"unix:///var/run/docker.sock".into()]);}
        if crate::which("podman") {
            if std::env::var_os("HOME").is_some_and(|home|PathBuf::from(home).join(".local/share/containers/storage").is_dir()) {out.push(vec!["podman".into(),"--remote=false".into()]);}
            if unsafe {libc::geteuid()}==0 {
                if let Ok(passwd)=fs::read_to_string("/etc/passwd") {
                    for line in passwd.lines().take(1024) {
                        let parts:Vec<_>=line.split(':').collect();if parts.len()!=7 {continue;}
                        let user=parts[0];let home=Path::new(parts[5]);
                        if user.bytes().all(|b|b.is_ascii_alphanumeric()||b"_-".contains(&b))&&home.join(".local/share/containers/storage").is_dir() {
                            if let Ok(uid)=parts[2].parse::<u32>() {if uid>0 {out.push(vec!["runuser".into(),"-u".into(),user.into(),"--".into(),"env".into(),format!("XDG_RUNTIME_DIR=/run/user/{uid}"),"podman".into(),"--remote=false".into()]);}}
                        }
                    }
                }
            }
        }
    }
    #[cfg(target_os="macos")] if let Some(home)=std::env::var_os("HOME") {
        for path in [PathBuf::from(&home).join(".docker/run/docker.sock"),PathBuf::from("/var/run/docker.sock")] {
            if path.exists() {out.push(vec!["docker".into(),"--host".into(),format!("unix://{}",path.display())]);break;}
        }
        if crate::which("podman") {out.push(vec!["podman".into(),"--remote=false".into()]);}
    }
    #[cfg(windows)] if crate::which("docker") {out.push(vec!["docker".into(),"--host".into(),"npipe:////./pipe/docker_engine".into()]);}
    out
}
pub fn runtime_label(runtime:&[String])->String {
    if runtime.first().is_some_and(|s|s=="runuser") {format!("podman:{}",runtime[2])}else{runtime[0].clone()}
}
pub fn probe(runtime:&[String],args:&[&str])->Result<String,String> {
    let argv:Vec<&str>=runtime.iter().map(String::as_str).chain(args.iter().copied()).collect();
    crate::command(&argv)
}
pub fn containers(runtime:&[String])->Result<Vec<Value>,String> {
    let listed=probe(runtime,&["container","ls","--all","--quiet","--no-trunc"])?;
    let ids:Vec<_>=listed.split_whitespace().collect();
    if ids.iter().any(|id|id.len()!=64||!id.bytes().all(|c|c.is_ascii_hexdigit())) {return Err("invalid container id".into());}
    let mut rows=vec![];
    for batch in ids.chunks(64) {
        let mut args=vec!["container","inspect","--format",r#"{"id":{{json .Id}},"name":{{json .Name}},"status":{{json .State.Status}},"pid":{{json .State.Pid}},"image":{{json .Config.Image}},"mounts":{{json .Mounts}}}"#];
        args.extend_from_slice(batch);
        let text=probe(runtime,&args)?;
        let parsed:Result<Vec<Value>,_>=text.lines().map(serde_json::from_str).collect();
        let parsed=parsed.map_err(|_|"invalid container metadata")?;
        if parsed.len()!=batch.len() {return Err("incomplete container metadata".into());}
        rows.extend(parsed);
    }
    Ok(rows)
}
pub fn container_name(runtime:&[String],row:&Value)->String {
    format!("{}:{}",runtime_label(runtime),row["name"].as_str().unwrap_or("unknown").trim_start_matches('/'))
}

pub fn collect(rows:&mut Vec<Value>,measure:impl Fn(&mut Value,u32))->Value {
    let mut count=0; let mut errors=0;
    for runtime in runtimes() {
        match containers(&runtime) {
            Ok(containers)=>for container in containers {
                let name=container_name(&runtime,&container);
                let mut row=json!({"entity":"container","identity":name,"status":container["status"],"cpu_used_milli":null,"ram_used_bytes":null,"disk_used_bytes":null,
                    "unknown":{"cpu_used_milli":"not-attributable","ram_used_bytes":"not-attributable","disk_used_bytes":"mounts-reported-separately"}});
                if matches!(container["status"].as_str(),Some("exited"|"created"|"dead")) {
                    row["cpu_used_milli"]=0.into();row["ram_used_bytes"]=0.into();row["unknown"].as_object_mut().unwrap().remove("cpu_used_milli");row["unknown"].as_object_mut().unwrap().remove("ram_used_bytes");
                } else if cfg!(target_os="linux") {
                    if let Some(pid)=container["pid"].as_u64().and_then(|p|u32::try_from(p).ok()).filter(|p|*p>0) {measure(&mut row,pid);}
                }
                rows.push(row);
                count+=1;
                if let Some(image)=container["image"].as_str() {
                    simple(rows,"image",image,"observed");link(rows,"image",("container",&name),("image",image));
                }
                for mount in container["mounts"].as_array().into_iter().flatten() {
                    if let Some(source)=mount["Source"].as_str().filter(|p|cfg!(target_os="linux")&&Path::new(p).is_absolute()) {
                        rows.push(json!({"entity":"folder","identity":source,"status":null,"disk_used_bytes":null,"unknown":{"status":"not-measured","disk_used_bytes":"not-measured"}}));
                        link(rows,"uses",("container",&name),("folder",source));
                    }
                }
            },
            Err(_)=>errors+=1,
        }
    }
    sites(rows);
    if errors>0 {json!({"status":"unknown","reason":"runtime-unavailable","count":null})} else {json!({"status":"ok","count":count})}
}

pub fn roots()->Vec<PathBuf> {
    #[cfg(target_os="linux")] let mut paths:Vec<PathBuf>=["/srv","/var/www","/opt","/home","/var/lib"].iter().map(PathBuf::from).collect();
    #[cfg(target_os="macos")] let mut paths=vec![PathBuf::from("/Users"),PathBuf::from("/Applications"),PathBuf::from("/Library")];
    #[cfg(windows)] let mut paths:Vec<PathBuf>=std::env::var_os("SystemDrive").map(|d|vec![PathBuf::from(format!("{}\\",d.to_string_lossy()))]).unwrap_or_default();
    #[cfg(target_os="linux")] if let Ok(mounts)=fs::read_to_string("/proc/self/mountinfo") {
        for line in mounts.lines() {
            if let Some((left,right))=line.split_once(" - ") {
                if right.split_whitespace().next().is_some_and(|fs|["ext2","ext3","ext4","xfs","btrfs","zfs"].contains(&fs)) {
                    if let Some(path)=left.split_whitespace().nth(4).filter(|p|*p!="/") {
                        let path=PathBuf::from(path.replace("\\040"," ").replace("\\134","\\"));if !paths.contains(&path){paths.push(path);}
                    }
                }
            }
        }
    }
    paths.retain(|p|fs::symlink_metadata(p).is_ok_and(|m|m.is_dir()&&!m.file_type().is_symlink()));
    paths
}

fn domain(s:&str)->bool {
    !s.is_empty()&&s.len()<=253&&s.bytes().all(|b|b.is_ascii_alphanumeric()||b".-*".contains(&b))&&s!="_"
}
fn tokens(text:&str,caddy:bool)->Option<Vec<String>> {
    let (mut out,mut token,mut quoted,mut escaped,mut comment)=(vec![],String::new(),None,false,false);
    for c in text.chars() {
        if comment {if c=='\n' {comment=false;} else {continue;}}
        if escaped {token.push(c);escaped=false;continue;}
        if c=='\\' {escaped=true;continue;}
        if let Some(q)=quoted {if c==q {quoted=None;}else{token.push(c);}continue;}
        if c=='\''||c=='"' {quoted=Some(c);continue;}
        if c=='#' {comment=true;continue;}
        if c.is_whitespace()||"{};".contains(c) {
            if !token.is_empty() {out.push(std::mem::take(&mut token));}
            if "{};".contains(c) {out.push(c.to_string());} else if caddy&&c=='\n' {out.push(";".into());}
        }else{token.push(c);}
    }
    if quoted.is_some()||escaped{return None;}
    if !token.is_empty(){out.push(token);}
    Some(out)
}
fn config(rows:&mut Vec<Value>,text:&str,caddy:bool) {
    let Some(tokens)=tokens(text,caddy) else{return;};
    let(mut depth,mut active,mut names,mut targets,mut statement)=(0usize,None,Vec::<String>::new(),Vec::<(String,String)>::new(),Vec::<String>::new());
    for token in tokens {
        match token.as_str() {
            "{"=>{
                depth+=1;
                if active.is_none()&&((!caddy&&statement==["server"])||(caddy&&statement.iter().any(|t|domain(t)&&t.contains('.')))) {
                    active=Some(depth);
                    if caddy {names=statement.iter().filter(|t|domain(t)).cloned().collect();}
                }
                statement.clear();
            }
            "}"=>{
                if active==Some(depth) {
                    for name in &names {simple(rows,"site",name,"observed");for (kind,target) in &targets {if kind=="endpoint" {simple(rows,"endpoint",target,"observed");}link(rows,"uses",("site",name),(kind,target));}}
                    names.clear();targets.clear();active=None;
                }
                depth=depth.saturating_sub(1);statement.clear();
            }
            ";"=>{
                if active.is_some() {
                    if statement.first().is_some_and(|s|s=="server_name") {names.extend(statement.iter().skip(1).filter(|s|domain(s)).cloned());}
                    if statement.first().is_some_and(|s|s=="proxy_pass"||s=="reverse_proxy") {
                        for value in statement.iter().skip(1) {
                            let endpoint=value.strip_prefix("http://").or_else(||value.strip_prefix("https://")).unwrap_or(value).trim_end_matches('/');
                            if endpoint.len()<=256&&endpoint.contains(':')&&endpoint.bytes().all(|b|b.is_ascii_alphanumeric()||b".:-_[]".contains(&b)) {targets.push(("endpoint".into(),endpoint.into()));}
                        }
                    }
                    if statement.first().is_some_and(|s|s=="root"||s=="alias") {
                        if let Some(path)=statement.iter().skip(1).find(|s|Path::new(s).is_absolute()&&!s.contains('$')) {targets.push(("folder".into(),path.clone()));}
                    }
                }
                statement.clear();
            }
            _=>statement.push(token),
        }
    }
}
fn sites(rows:&mut Vec<Value>) {
    let mut files=vec![PathBuf::from("/etc/nginx/nginx.conf"),PathBuf::from("/etc/caddy/Caddyfile")];
    for dir in ["/etc/nginx/sites-enabled","/etc/nginx/conf.d","/etc/caddy/conf.d"] {
        if let Ok(entries)=fs::read_dir(dir) {files.extend(entries.take(128).flatten().map(|e|e.path()));}
    }
    for file in files {
        // Only explicit administrator configuration locations; bytes never enter a report or telemetry.
        if fs::metadata(&file).is_ok_and(|m|m.is_file()&&m.len()<=128*1024) {
            if let Ok(text)=fs::read_to_string(&file) {config(rows,&text,file.to_string_lossy().contains("caddy"));}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn domain_links_keep_server_blocks_separate_and_never_export_config_values() {
        let mut rows=vec![];
        let root=if cfg!(windows){"C:/srv"}else{"/srv"};
        config(&mut rows,&format!("http {{ server {{ server_name a.example; root {root}/a; location / {{ proxy_set_header Secret invented-secret; }} }} server {{ server_name b.example; root {root}/b; }} }}"),false);
        assert_eq!(rows.iter().filter(|r|r["entity"]=="site").count(),2);
        assert!(rows.iter().any(|r|r["from_identity"]=="a.example"&&r["to_identity"]==format!("{root}/a")));
        assert!(!rows.iter().any(|r|r["from_identity"]=="a.example"&&r["to_identity"]==format!("{root}/b")));
        assert!(!json!(rows).to_string().contains("invented-secret"));
        let mut rows=vec![];config(&mut rows,&format!("c.example {{\n root * {root}/c\n reverse_proxy 127.0.0.1:8080\n}}\n"),true);
        assert!(rows.iter().any(|r|r["from_identity"]=="c.example"&&r["to_identity"]==format!("{root}/c")));
        assert!(rows.iter().any(|r|r["from_identity"]=="c.example"&&r["to_entity"]=="endpoint"&&r["to_identity"]=="127.0.0.1:8080"));
    }
}
