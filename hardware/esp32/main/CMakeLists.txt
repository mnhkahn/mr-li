set(s3_sources)
if(IDF_TARGET STREQUAL "esp32s3")
    list(APPEND s3_sources "voice.cc" "light_ring.cc" "light_music.cc")
endif()
idf_component_register(SRCS "main.cc" "xgimi_wake.cc" "curtain.cc" "network.cc" "rf_learning.cc" ${s3_sources}
    INCLUDE_DIRS "."
    PRIV_REQUIRES bt nvs_flash json esp_driver_usb_serial_jtag esp_driver_rmt esp_driver_gpio esp_driver_pcnt
        esp_driver_uart esp_driver_i2s esp_psram esp_wifi esp_event esp_netif esp_http_server esp_timer esp_system esp_hw_support mdns)
