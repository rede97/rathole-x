const fs = require('fs');

// websocket.rs
{
  const p = 'src/transport/websocket.rs';
  let s = fs.readFileSync(p, 'utf8');
  s = s.replace('#[derive(Debug)]\nenum TransportStream {',
`#[derive(Debug)]
#[allow(clippy::large_enum_variant)] // upstream shape; boxing would churn call sites
enum TransportStream {`);
  s = s.split(`            Err(e) => Poll::Ready(Err(Error::new(ErrorKind::Other, e))),`)
       .join(`            Err(e) => Poll::Ready(Err(Error::other(e))),`);
  fs.writeFileSync(p, s, 'utf8');
  console.log('websocket ok');
}

// windows.rs to_vec
{
  const p = 'src/platform/windows.rs';
  let s = fs.readFileSync(p, 'utf8');
  const out = s.split('    let mut params: Vec<String> = args.iter().cloned().collect();')
               .join('    let mut params: Vec<String> = args.to_vec();');
  fs.writeFileSync(p, out, 'utf8');
  console.log('windows ok');
}

// config_edit test: drop pointless set_readonly(false)
{
  const p = 'src/config_edit.rs';
  let s = fs.readFileSync(p, 'utf8');
  const out = s.split(`            assert!(!writable_by_current_user(&config), "read-only file not writable");
            let mut perms = std::fs::metadata(&config).unwrap().permissions();
            perms.set_readonly(false);
            std::fs::set_permissions(&config, perms).unwrap();
        }`)
 .join(`            assert!(!writable_by_current_user(&config), "read-only file not writable");
        }`);
  fs.writeFileSync(p, out, 'utf8');
  console.log('config_edit ok');
}
