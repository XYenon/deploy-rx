# SPDX-FileCopyrightText: 2020 Serokell <https://serokell.io/>
#
# SPDX-License-Identifier: MPL-2.0

{
  users.users.admin = {
    isNormalUser = true;
    extraGroups = [
      "wheel"
      "sudo"
    ];
    password = "123";
  };

  services.openssh.enable = true;

  # Another option would be root on the server
  security.sudo.extraRules = [
    {
      groups = [ "wheel" ];
      commands = [
        {
          command = "ALL";
          options = [ "NOPASSWD" ];
        }
      ];
    }
  ];

  nix.settings = {
    # Allow users in the wheel group to upload unsigned NARs.
    trusted-users = [ "@wheel" ];
    trusted-public-keys = [ "cache.nixos.org-1:6NCHdD59X431o0gWypbMrAURkbJ16ZPMQFGspcDShjY=" ];
  };

  # These settings provide a /boot filesystem in the VM.
  boot.loader = {
    systemd-boot.enable = true;
    efi.canTouchEfiVariables = true;
  };

  virtualisation = {
    useBootLoader = true;
    writableStore = true;
    useEFIBoot = true;
  };
}
