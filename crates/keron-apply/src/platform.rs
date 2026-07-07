#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OsFamily {
    Linux,
    Macos,
    Windows,
    Unknown,
}

impl OsFamily {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Linux => "Linux",
            Self::Macos => "Macos",
            Self::Windows => "Windows",
            Self::Unknown => "Unknown",
        }
    }
}

pub const fn map_os_type(t: os_info::Type) -> OsFamily {
    use os_info::Type;
    match t {
        Type::Linux
        | Type::AlmaLinux
        | Type::Alpaquita
        | Type::Alpine
        | Type::ALTLinux
        | Type::Amazon
        | Type::Android
        | Type::AOSC
        | Type::Arch
        | Type::Artix
        | Type::Bazzite
        | Type::Bluefin
        | Type::CachyOS
        | Type::CentOS
        | Type::Debian
        | Type::Elementary
        | Type::EndeavourOS
        | Type::Fedora
        | Type::Garuda
        | Type::Gentoo
        | Type::InstantOS
        | Type::Kali
        | Type::KDENeon
        | Type::Mabox
        | Type::Manjaro
        | Type::Mariner
        | Type::Mint
        | Type::NixOS
        | Type::Nobara
        | Type::OpenCloudOS
        | Type::openEuler
        | Type::openSUSE
        | Type::OracleLinux
        | Type::PikaOS
        | Type::Pop
        | Type::Raspbian
        | Type::Redhat
        | Type::RedHatEnterprise
        | Type::RockyLinux
        | Type::Solus
        | Type::SUSE
        | Type::Ubuntu
        | Type::Ultramarine
        | Type::Uos
        | Type::Void
        | Type::Zorin => OsFamily::Linux,
        Type::Macos => OsFamily::Macos,
        Type::Windows => OsFamily::Windows,
        _ => OsFamily::Unknown,
    }
}

#[cfg_attr(test, mutants::skip)]
pub fn detect_os_family() -> OsFamily {
    #[cfg(test)]
    if let Some(os) = test_override() {
        return os;
    }
    map_os_type(os_info::get().os_type())
}

#[cfg(test)]
thread_local! {
    static OS_OVERRIDE: std::cell::Cell<Option<OsFamily>> = const { std::cell::Cell::new(None) };
}

#[cfg(test)]
fn test_override() -> Option<OsFamily> {
    OS_OVERRIDE.with(std::cell::Cell::get)
}

#[cfg(test)]
pub struct OsOverride {
    previous: Option<OsFamily>,
}

#[cfg(test)]
impl OsOverride {
    pub fn set(os: OsFamily) -> Self {
        let previous = OS_OVERRIDE.with(|slot| {
            let previous = slot.get();
            slot.set(Some(os));
            previous
        });
        Self { previous }
    }
}

#[cfg(test)]
impl Drop for OsOverride {
    fn drop(&mut self) {
        OS_OVERRIDE.with(|slot| slot.set(self.previous));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn os_family_label_matches_public_union_variants() {
        assert_eq!(OsFamily::Linux.label(), "Linux");
        assert_eq!(OsFamily::Macos.label(), "Macos");
        assert_eq!(OsFamily::Windows.label(), "Windows");
        assert_eq!(OsFamily::Unknown.label(), "Unknown");
    }

    #[test]
    fn map_os_type_categorizes_every_linux_flavour() {
        use os_info::Type;
        for t in [
            Type::Linux,
            Type::AlmaLinux,
            Type::Alpaquita,
            Type::Alpine,
            Type::ALTLinux,
            Type::Amazon,
            Type::Android,
            Type::AOSC,
            Type::Arch,
            Type::Artix,
            Type::Bazzite,
            Type::Bluefin,
            Type::CachyOS,
            Type::CentOS,
            Type::Debian,
            Type::Elementary,
            Type::EndeavourOS,
            Type::Fedora,
            Type::Garuda,
            Type::Gentoo,
            Type::InstantOS,
            Type::Kali,
            Type::KDENeon,
            Type::Mabox,
            Type::Manjaro,
            Type::Mariner,
            Type::Mint,
            Type::NixOS,
            Type::Nobara,
            Type::OpenCloudOS,
            Type::openEuler,
            Type::openSUSE,
            Type::OracleLinux,
            Type::PikaOS,
            Type::Pop,
            Type::Raspbian,
            Type::Redhat,
            Type::RedHatEnterprise,
            Type::RockyLinux,
            Type::Solus,
            Type::SUSE,
            Type::Ubuntu,
            Type::Ultramarine,
            Type::Uos,
            Type::Void,
            Type::Zorin,
        ] {
            assert_eq!(
                map_os_type(t),
                OsFamily::Linux,
                "expected `{t:?}` to map to Linux"
            );
        }
    }

    #[test]
    fn map_os_type_categorizes_macos_and_windows() {
        assert_eq!(map_os_type(os_info::Type::Macos), OsFamily::Macos);
        assert_eq!(map_os_type(os_info::Type::Windows), OsFamily::Windows);
    }

    #[test]
    fn map_os_type_falls_back_to_unknown_for_unmapped_variants() {
        assert_eq!(map_os_type(os_info::Type::Unknown), OsFamily::Unknown);
        assert_eq!(map_os_type(os_info::Type::FreeBSD), OsFamily::Unknown);
        assert_eq!(map_os_type(os_info::Type::DragonFly), OsFamily::Unknown);
    }

    #[test]
    fn detect_os_family_honours_test_override() {
        let _guard = OsOverride::set(OsFamily::Windows);
        assert_eq!(detect_os_family(), OsFamily::Windows);
    }

    #[test]
    fn detect_os_family_override_restores_previous_value() {
        let outer = OsOverride::set(OsFamily::Linux);
        {
            let _inner = OsOverride::set(OsFamily::Macos);
            assert_eq!(detect_os_family(), OsFamily::Macos);
        }
        assert_eq!(detect_os_family(), OsFamily::Linux);
        drop(outer);
    }
}
