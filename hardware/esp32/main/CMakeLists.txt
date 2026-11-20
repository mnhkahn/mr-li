idf_component_register(SRCS "main.cc" "xgimi_wake.cc" "curtain.cc" "network.cc"
    INCLUDE_DIRS "."
    PRIV_REQUIRES bt nvs_flash json esp_driver_usb_serial_jtag esp_driver_rmt esp_driver_gpio esp_wifi esp_event esp_netif esp_http_server esp_timer esp_system esp_hw_support mdns)
