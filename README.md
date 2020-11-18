## Motivation

Provide a method to access intranet services via a internet server, 
namely intranet penetration.

## Design Overview

Port Forwarder include two executable file, intranet provider and internet provider,
following will refer as CLIENT and SERVER respectively.

``` c
/**
                                        |
           ----------------             |               -----------------------            -------------
  services |              |        A1.REQUEST           |                     |            |           |
 listening |              | <---------- | ------------- |                     |       port |  SERVICE  |
     ports |              |         TCP CONNECTION      |                     |           x|           |
          x|    SERVER    | ----------- | ------------> |       CLIENT        |            -------------
          x|    PROCESS   |    A2.ESTABLISH CONNECTION  |       PROCESS       |                  .
          x|              |             |               |                     |                  .
          x|              |             |               |                     |                  .
          x|              |             |               |                     |                  .
          x|              |             |               |                     |                  .
 B1.TCP   x|   B2.        |        B3.NOTIFY            |                     |            -------------
 CONNECT  x|   LOOKUP     | ----------- | ------------> |    B4.TRY TO        |            |           |
 -------> x|   CONNECTION |             |               |     CONNECT TO      |       port |  SERVICE  |
          x|              |     B6.NEW CONNECTION       |     SERVER          | --------> x|           |
          x|              | <-------------------------- |                     |  B5.TCP    -------------
          x|   B8.        | --------------------------> |                     |     CONNECTION   .
          x|   START      |     B7.ACCEPT               |                     |                  .
          x|   FORWARDING |             |               |                     |                  .
          x|              |             |               |                     |                  .
          x|              |             |               |                     |                  .
 C1.UDP   x|              |             |               |                     |                  .
 SEND     x|              |             |               |                     |                  .
 -------> x|   SIMILAR    |             |               |                     |                  .
          x|              |             |               |                     |                  .
          x|              |             |               |                     |                  .
           ----------------             |               -----------------------
                                        |
         Internet Accessible            |
               Host                     |
                                        |
                                        |
*/
```

## Desired Features

> Multiple TCP and UDP port forwarding
> User authentication
> Configuration File
> Control API via HTTP PRC
> Web frontend UI
> Connection encryption ???

