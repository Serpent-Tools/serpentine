//! Code for parsing unix user identifiers as well as relevant system files to resolve to a
//! complete `oci_spec` User as well as finding the home directory.

use std::str::FromStr;

use crate::engine::RuntimeError;

/// The possible ways to identify a unix user.
#[derive(Debug, Clone, PartialEq, Eq)]
enum UserIdentifier {
    /// The internal user id
    Id(u32),
    /// The username
    Name(Box<str>),
}

impl FromStr for UserIdentifier {
    type Err = std::convert::Infallible;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.parse() {
            Ok(id) => Ok(Self::Id(id)),
            Err(_) => Ok(Self::Name(s.into())),
        }
    }
}

/// The possible ways to identify a unix group.
#[derive(Debug, Clone, PartialEq, Eq)]
enum GroupIdentifier {
    /// The internal group id
    Id(u32),
    /// The group name
    Name(Box<str>),
}

impl FromStr for GroupIdentifier {
    type Err = std::convert::Infallible;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.parse() {
            Ok(id) => Ok(Self::Id(id)),
            Err(_) => Ok(Self::Name(s.into())),
        }
    }
}

/// Represents a parsed user string from a oci spec, might or might not contain a explicit group.
///
/// Valid formats are: `user`, `uid`, `user:group`, `uid:gid`, `uid:group`, `user:gid`
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OciUser {
    /// The user
    user: UserIdentifier,
    /// The primary group
    group: Option<GroupIdentifier>,
}

impl FromStr for OciUser {
    type Err = std::convert::Infallible; // We never parse non-valid strings, in theory.

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if let Some((user, group)) = s.split_once(':') {
            let Ok(user) = user.parse();
            let Ok(group) = group.parse();
            Ok(Self {
                user,
                group: Some(group),
            })
        } else {
            let Ok(user) = s.parse();
            Ok(Self { user, group: None })
        }
    }
}

/// A entry in the `/etc/passwd`
/// (This struct only contains filed relevant to our task.)
#[derive(Debug, Clone, PartialEq, Eq)]
struct PasswdEntry {
    /// The username of the account
    username: Box<str>,
    /// The user id
    user_id: u32,
    /// the group id
    group_id: u32,
    /// The users home directory
    home_directory: Box<str>,
}

impl FromStr for PasswdEntry {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let mut fields = s.split(':');

        let username = fields.next().ok_or(())?;
        let _password = fields.next().ok_or(())?;
        let uid = fields.next().ok_or(())?;
        let gid = fields.next().ok_or(())?;
        let _comments = fields.next().ok_or(())?;
        let home_dir = fields.next().ok_or(())?;
        let _shell = fields.next().ok_or(())?;

        Ok(Self {
            username: username.into(),
            user_id: uid.parse().map_err(|_| ())?,
            group_id: gid.parse().map_err(|_| ())?,
            home_directory: home_dir.into(),
        })
    }
}

/// A passwd file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Passwd(Box<[PasswdEntry]>);

impl FromStr for Passwd {
    type Err = std::convert::Infallible;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let mut result = Vec::new();
        for line in s.lines() {
            if let Ok(entry) = line.parse() {
                result.push(entry);
            }
        }
        Ok(Self(result.into()))
    }
}

/// A entry in `/etc/group`
#[derive(Debug, Clone, PartialEq, Eq)]
struct GroupEntry {
    /// The name of the group
    group_name: Box<str>,
    /// The id of the group
    group_id: u32,
    /// The list of user(names) of the members of the group, this is used to find additional groups a user is a member of outside
    /// their primary.
    members: Box<[Box<str>]>,
}

impl FromStr for GroupEntry {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let mut fields = s.split(':');

        let group_name = fields.next().ok_or(())?;
        let _password = fields.next().ok_or(())?;
        let group_id = fields.next().ok_or(())?;
        let members = fields.next().ok_or(())?;

        let members = members
            .split(',')
            .filter(|user| !user.is_empty())
            .map(Into::into)
            .collect::<Vec<_>>()
            .into();

        Ok(Self {
            group_name: group_name.into(),
            group_id: group_id.parse().map_err(|_| ())?,
            members,
        })
    }
}

/// The entries in the `/etc/group` file
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EtcGroup(Box<[GroupEntry]>);

impl FromStr for EtcGroup {
    type Err = std::convert::Infallible;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let mut result = Vec::new();
        for line in s.lines() {
            if let Ok(entry) = line.parse() {
                result.push(entry);
            }
        }
        Ok(Self(result.into()))
    }
}

impl Passwd {
    /// Find a user in the passwd file from the given user idenitifer
    fn find_user(self, identifier: &UserIdentifier) -> Option<PasswdEntry> {
        self.0.into_iter().find(|entry| match identifier {
            UserIdentifier::Name(name) => entry.username == *name,
            UserIdentifier::Id(id) => entry.user_id == *id,
        })
    }
}

impl EtcGroup {
    /// Verify that the given id exists in /etc/group
    fn check_gid_valid(&self, gid: u32) -> bool {
        self.0.iter().any(|entry| entry.group_id == gid)
    }

    /// Find a group by its name
    fn find_group_by_name(&self, group_name: &str) -> Option<&GroupEntry> {
        self.0.iter().find(|entry| &*entry.group_name == group_name)
    }

    /// Return a iterator over all groups that contain the given username as a member
    fn get_groups_user_is_member_of(&self, username: &str) -> impl Iterator<Item = &GroupEntry> {
        self.0
            .iter()
            .filter(move |entry| entry.members.iter().any(|member| &**member == username))
    }
}

impl OciUser {
    /// Uses data from `/etc/passwd` and `/etc/group` to construct a runc user struct, as well as
    /// returns the users home directory.
    pub fn resolve(
        self,
        passwd: Passwd,
        groups: &EtcGroup,
    ) -> Result<(oci_spec::runtime::User, Box<str>), RuntimeError> {
        let mut user = oci_spec::runtime::User::default();

        let Some(passwd_entry) = passwd.find_user(&self.user) else {
            return Err(RuntimeError::UserNotFound {
                user: self,
                msg: "Failed to find user in /etc/passwd",
            });
        };

        user.set_uid(passwd_entry.user_id);

        match &self.group {
            Some(GroupIdentifier::Id(id)) => {
                if !groups.check_gid_valid(*id) {
                    return Err(RuntimeError::UserNotFound {
                        user: self,
                        msg: "Group id not found in /etc/group",
                    });
                }

                user.set_gid(*id);
                user.set_additional_gids(Some(vec![]));
            }
            Some(GroupIdentifier::Name(group_name)) => {
                let Some(group) = groups.find_group_by_name(group_name) else {
                    return Err(RuntimeError::UserNotFound {
                        user: self,
                        msg: "Group name not found in /etc/group",
                    });
                };

                user.set_gid(group.group_id);
                user.set_additional_gids(Some(vec![]));
            }
            None => {
                let group_id = passwd_entry.group_id;
                if !groups.check_gid_valid(group_id) {
                    return Err(RuntimeError::UserNotFound {
                        user: self,
                        msg: "Group id not found in /etc/group",
                    });
                }

                user.set_gid(group_id);
                let additional_groups = groups
                    .get_groups_user_is_member_of(&passwd_entry.username)
                    .filter(|entry| entry.group_id != group_id)
                    .map(|entry| entry.group_id)
                    .collect::<Vec<_>>();
                user.set_additional_gids(Some(additional_groups));
            }
        }

        Ok((user, passwd_entry.home_directory))
    }
}

#[cfg(test)]
mod tests {
    use oci_spec::runtime::UserBuilder;
    use rstest::rstest;

    use super::*;

    #[rstest]
    #[case::username("viv", OciUser { user: UserIdentifier::Name("viv".into()), group: None })]
    #[case::uid("123", OciUser { user: UserIdentifier::Id(123), group: None })]
    #[case::username_groupname("viv:docker", OciUser { user: UserIdentifier::Name("viv".into()), group: Some(GroupIdentifier::Name("docker".into())) })]
    #[case::username_gid("viv:123", OciUser { user: UserIdentifier::Name("viv".into()), group: Some(GroupIdentifier::Id(123)) })]
    #[case::uid_groupname("666:docker", OciUser { user: UserIdentifier::Id(666), group: Some(GroupIdentifier::Name("docker".into())) })]
    #[case::uid_gid("666:123", OciUser { user: UserIdentifier::Id(666), group: Some(GroupIdentifier::Id(123)) })]
    fn oci_user_parsing(#[case] source: &str, #[case] expected: OciUser) {
        let Ok(user): Result<OciUser, _> = source.parse();

        assert_eq!(user, expected);
    }

    #[test]
    fn passwd_parsing() {
        let file_contents = "
root:x:0:0:root:/root:/bin/bash
daemon:x:1:1:daemon:/usr/sbin:/usr/sbin/nologin
viv:x:1000:2000:Viv:/home/viv:/bin/bash
";

        let Ok(parsed_file): Result<Passwd, _> = file_contents.parse();

        assert_eq!(
            parsed_file.0.first(),
            Some(&PasswdEntry {
                username: "root".into(),
                user_id: 0,
                group_id: 0,
                home_directory: "/root".into()
            })
        );
        assert_eq!(
            parsed_file.0.get(1),
            Some(&PasswdEntry {
                username: "daemon".into(),
                user_id: 1,
                group_id: 1,
                home_directory: "/usr/sbin".into()
            })
        );
        assert_eq!(
            parsed_file.0.get(2),
            Some(&PasswdEntry {
                username: "viv".into(),
                user_id: 1000,
                group_id: 2000,
                home_directory: "/home/viv".into()
            })
        );
        assert_eq!(parsed_file.0.get(3), None,);
    }

    #[test]
    fn group_parsing() {
        let file_contents = "
root:x:0:
adm:x:4:viv
cool:x:34:viv,ferris
";

        let Ok(parsed_file): Result<EtcGroup, _> = file_contents.parse();

        assert_eq!(
            parsed_file.0.first(),
            Some(&GroupEntry {
                group_name: "root".into(),
                group_id: 0,
                members: [].into(),
            })
        );
        assert_eq!(
            parsed_file.0.get(1),
            Some(&GroupEntry {
                group_name: "adm".into(),
                group_id: 4,
                members: ["viv".into()].into(),
            })
        );
        assert_eq!(
            parsed_file.0.get(2),
            Some(&GroupEntry {
                group_name: "cool".into(),
                group_id: 34,
                members: ["viv".into(), "ferris".into()].into(),
            })
        );
        assert_eq!(parsed_file.0.get(3), None);
    }

    #[rstest]
    #[case::root_username(
        "root",
        UserBuilder::default().uid(0_u32).gid(0_u32).additional_gids([]),
        "/root"
    )]
    #[case::root_id(
        "0",
        UserBuilder::default().uid(0_u32).gid(0_u32).additional_gids([]),
        "/root"
    )]
    #[case::ferris_username(
        "ferris",
        UserBuilder::default().uid(1001_u32).gid(34_u32).additional_gids([]),
        "/home/ferris"
    )]
    #[case::ferris_id(
        "1001",
        UserBuilder::default().uid(1001_u32).gid(34_u32).additional_gids([]),
        "/home/ferris"
    )]
    #[case::viv_username(
        "viv",
        UserBuilder::default().uid(1000_u32).gid(4_u32).additional_gids([34]),
        "/home/viv"
    )]
    #[case::viv_id(
        "1000",
        UserBuilder::default().uid(1000_u32).gid(4_u32).additional_gids([34]),
        "/home/viv"
    )]
    #[case::viv_group_id(
        "viv:0",
        UserBuilder::default().uid(1000_u32).gid(0_u32).additional_gids([]),
        "/home/viv"
    )]
    #[case::viv_group_name(
        "viv:root",
        UserBuilder::default().uid(1000_u32).gid(0_u32).additional_gids([]),
        "/home/viv"
    )]
    fn resolve(
        #[case] user_string: &str,
        #[case] expected_user: UserBuilder,
        #[case] expected_home: &str,
    ) {
        let Ok(passwd): Result<Passwd, _> = "
root:x:0:0:root:/root:/bin/bash
ferris:x:1001:34:daemon:/home/ferris:/bin/bash
viv:x:1000:4:Viv:/home/viv:/bin/bash
"
        .parse();

        let Ok(groups): Result<EtcGroup, _> = "
root:x:0:
adm:x:4:viv
cool:x:34:viv,ferris
"
        .parse();

        let Ok(user): Result<OciUser, _> = user_string.parse();

        let (full_user, home_directory) = user.resolve(passwd, &groups).unwrap();

        assert_eq!(full_user, expected_user.build().unwrap());
        assert_eq!(&*home_directory, expected_home);
    }

    #[rstest]
    #[case::missing_username("snek")]
    #[case::missing_uid("100")]
    #[case::missing_groupname("viv:docker")]
    #[case::missing_gid("viv:100")]
    fn resolve_error_cases(#[case] user_string: &str) {
        let Ok(passwd): Result<Passwd, _> = "
root:x:0:0:root:/root:/bin/bash
ferris:x:1001:34:daemon:/home/ferris:/bin/bash
viv:x:1000:4:Viv:/home/viv:/bin/bash
"
        .parse();

        let Ok(groups): Result<EtcGroup, _> = "
root:x:0:
adm:x:4:viv
cool:x:34:viv,ferris
"
        .parse();

        let Ok(user): Result<OciUser, _> = user_string.parse();

        let result = user.resolve(passwd, &groups);

        assert!(result.is_err(), "Expected handling {user_string} to fail");
    }
}
