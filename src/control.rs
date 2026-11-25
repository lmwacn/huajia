use crate::{
    Result,
    cloud::{Client, PowerState},
    lan, session,
};

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Auto,
    Lan,
    Cloud,
}

pub fn cloud() -> Result<Client> {
    Ok(Client::new(session::Session::load(&session::path()?)?))
}

pub fn power(id: &str, requested: Option<bool>, mode: Mode) -> Result<PowerState<'_>> {
    let run_cloud = || {
        let client = cloud()?;
        match requested {
            Some(on) => client.set_power(id, on),
            None => client.get_power(id),
        }
    };
    if mode == Mode::Cloud {
        return run_cloud();
    }
    let Some(device) = lan::load(id)? else {
        return if mode == Mode::Auto {
            run_cloud()
        } else {
            Err(format!("设备 {id:?} 未导入局域网配置，请运行 huajia lan import <设备 ID>").into())
        };
    };
    // 先用只读请求验证设备身份和本地属性。写入一旦发出，便不再切换路径。
    let prepared = device.connect().and_then(|mut client| {
        let state = device.read(id, &mut client)?;
        Ok((client, state))
    });
    use_prepared(
        prepared,
        mode,
        |(mut client, state)| match requested {
            Some(on) if state.power != on => device.write(id, &mut client, on),
            _ => Ok(state),
        },
        run_cloud,
    )
}

fn use_prepared<T, R>(
    prepared: Result<T>,
    mode: Mode,
    local: impl FnOnce(T) -> Result<R>,
    cloud: impl FnOnce() -> Result<R>,
) -> Result<R> {
    match prepared {
        Ok(target) => local(target),
        Err(err) if mode == Mode::Auto => cloud().map_err(|cloud_err| {
            format!("局域网不可用（{err}），米家云也失败（{cloud_err}）").into()
        }),
        Err(err) => Err(err),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auto_fallback_applies_only_before_a_local_write() {
        assert_eq!(
            use_prepared(
                Err("异网".into()),
                Mode::Auto,
                |(): ()| panic!(),
                || Ok("cloud")
            )
            .unwrap(),
            "cloud"
        );
        assert!(use_prepared(Err("异网".into()), Mode::Lan, |(): ()| Ok(()), || panic!()).is_err());
        assert_eq!(
            use_prepared(
                Ok(()),
                Mode::Auto,
                |_| Err::<(), _>("写入未获确认".into()),
                || panic!()
            )
            .unwrap_err()
            .to_string(),
            "写入未获确认"
        );
    }
}
